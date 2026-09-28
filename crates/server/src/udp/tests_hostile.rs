// Temporary hostile-client harness for the UDP relay leg (round 2).
// Removed after the audit.
//
// Threat model: the peer holds a valid broker ticket for ONE session, so it
// knows that session's token and its c47/s2c AEAD keys and can mint as many
// valid datagrams as it likes. It cannot mint tokens/keys for any other
// session. The questions here are all of the form "what can this peer do to
// the *shared* relay, to OTHER sessions, or to third parties?".
use std::time::Duration;

use tokio::time::timeout;

use shared::{
    crypt::{
        datagram::{DatagramCrypt, MAX_DATAGRAM_PAYLOAD, random_token},
        tunnel::get_udp_crypts,
        types::SharedSecret,
    },
    protocol::{consts::TICKET_LENGTH, ticket::Ticket},
};

use super::*;

const T: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(400);

async fn host_relay() -> (Arc<UdpRelay>, Trigger, SocketAddr) {
    let relay = UdpRelay::bind_for_test("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = relay.local_addr().unwrap();
    let stop = Trigger::new();
    let r = relay.clone();
    let s = stop.clone();
    tokio::spawn(async move { r.run(s).await });
    (relay, stop, addr)
}

/// Build a session carrying a UDP leg whose keys derive from
/// `(shared_secret, ticket)`, exactly like `connection::connect` does.
///
/// `seed` distinguishes sessions: two sessions built with the same seed would
/// (correctly) share key material, since both HKDF inputs are the same, and a
/// datagram minted for one would verify for the other. Production sessions
/// always have distinct tickets and distinct ML-KEM shared secrets.
async fn udp_session(
    remotes: Vec<String>,
    seed: u8,
) -> (Arc<Session>, UdpToken, SharedSecret, Ticket) {
    udp_session_full(remotes, seed, "127.0.0.1:0").await
}

/// Same, but with an explicit TCP-peer ip for the session (`src_ip`), so a
/// test can make the UDP source differ from the tunnel peer.
async fn udp_session_full(
    remotes: Vec<String>,
    seed: u8,
    src_ip: &str,
) -> (Arc<Session>, UdpToken, SharedSecret, Ticket) {
    let shared_secret = SharedSecret::new([seed; 32]);
    let ticket = Ticket::new([seed; TICKET_LENGTH]);
    let (inbound, outbound) = get_udp_crypts(&shared_secret, &ticket).unwrap();
    let token = random_token();
    let session = Arc::new(Session::new(
        shared_secret.clone(),
        ticket,
        Trigger::new(),
        src_ip.parse().unwrap(),
        remotes,
    ));
    session.set_udp(UdpState::new(token, inbound, outbound));
    (session, token, shared_secret, ticket)
}

/// Launcher-side mirror of the server crypts: send on the c2s key, receive
/// on the s2c key.
fn launcher(secret: &SharedSecret, ticket: &Ticket) -> (DatagramCrypt, DatagramCrypt) {
    let (send, _) = get_udp_crypts(secret, ticket).unwrap();
    let (_, recv) = get_udp_crypts(secret, ticket).unwrap();
    (send, recv)
}

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

// ---------------------------------------------------------------------------
// 1. Panic hunt: any panic in handle_datagram kills the single relay task for
//    EVERY session, so a hostile datagram that panics is a global DoS.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn hostile_datagram_fuzz_never_kills_the_relay() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_session, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_session);

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut state = 0xDEAD_BEEFu64;

    // (a) every length in 0..=1400 with the real token and random bodies:
    //     exercises handle_datagram's < HEADER check and decrypt's size/AEAD
    //     checks with attacker-controlled neighbourhoods.
    for len in 0..=1400usize {
        let mut d = vec![0u8; len];
        for b in d.iter_mut() {
            *b = lcg(&mut state) as u8;
        }
        if len >= TOKEN_LENGTH {
            d[..TOKEN_LENGTH].copy_from_slice(&token);
        }
        // occasionally stamp a plausible seq near the real one
        if len >= DATAGRAM_HEADER_SIZE && lcg(&mut state).is_multiple_of(2) {
            let seq = (1u64 << 63) + (lcg(&mut state) % 4096);
            d[TOKEN_LENGTH..DATAGRAM_HEADER_SIZE].copy_from_slice(&seq.to_be_bytes());
        }
        let _ = attacker.send_to(&d, relay_addr).await;
    }

    // (b) unknown / zero tokens, min and boundary sizes
    for _ in 0..2000 {
        let t = random_token();
        let len = (lcg(&mut state) % 1500) as usize;
        let mut d = vec![0u8; len.max(TOKEN_LENGTH)];
        for b in d.iter_mut() {
            *b = lcg(&mut state) as u8;
        }
        if !lcg(&mut state).is_multiple_of(8) {
            d[..TOKEN_LENGTH].copy_from_slice(&t);
        } else {
            d[..TOKEN_LENGTH].copy_from_slice(&[0u8; TOKEN_LENGTH]);
        }
        let _ = attacker.send_to(&d, relay_addr).await;
    }

    // (c) empty UDP datagrams
    for _ in 0..50 {
        let _ = attacker.send_to(&[], relay_addr).await;
    }

    // Liveness proof: a genuine datagram must still be forwarded. If the
    // relay task had panicked, nothing would arrive at the remote.
    let (mut csend, _) = launcher(&secret, &ticket);
    let good = csend.encrypt(&token, b"alive").unwrap();
    attacker.send_to(&good, relay_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let (len, _) = timeout(T, rdp.recv_from(&mut buf))
        .await
        .expect("relay task died during the hostile fuzz")
        .unwrap();
    assert_eq!(&buf[..len], b"alive");
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 2. One session's failure to build a remote leg must not affect others.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn remote_leg_failure_is_contained_to_its_own_session() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;

    // "bad" session: broker gave it no remotes, so create_remote_leg always
    // fails (remotes().first() -> None).
    let (bad, bad_token, bad_secret, bad_ticket) = udp_session(vec![], 7).await;
    relay.register(&bad);
    // "good" session next to it.
    let (_good, good_token, good_secret, good_ticket) =
        udp_session(vec![rdp_addr.to_string()], 8).await;
    relay.register(&_good);

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut bsend, _) = launcher(&bad_secret, &bad_ticket);
    let d = bsend.encrypt(&bad_token, b"boom").unwrap();
    attacker.send_to(&d, relay_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Documented ordering: the client address is learned from the
    // authenticated datagram *before* the forward is attempted, so it is set
    // even when nothing could be forwarded.
    assert_eq!(
        bad.udp().unwrap().client_addr(),
        Some(attacker.local_addr().unwrap()),
        "client_addr must be learned from the authenticated datagram"
    );
    assert_eq!(relay.counters.forwarded.load(Ordering::Relaxed), 0);

    // The good session is unaffected.
    let (mut gsend, _) = launcher(&good_secret, &good_ticket);
    let d = gsend.encrypt(&good_token, b"ok").unwrap();
    attacker.send_to(&d, relay_addr).await.unwrap();
    let mut buf = [0u8; 64];
    let (len, _) = timeout(T, rdp.recv_from(&mut buf))
        .await
        .expect("relay stopped serving the healthy session")
        .unwrap();
    assert_eq!(&buf[..len], b"ok");
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 3. client_addr provenance: the return path must follow the wire source, not
//    anything an attacker can put in the payload.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn client_addr_comes_from_the_wire_source_not_the_payload() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);

    // Payload deliberately contains a victim-looking address. The relay must
    // ignore it: client_addr is only ever the source of the datagram.
    let d = csend.encrypt(&token, b"198.51.100.7:31337").unwrap();
    attacker.send_to(&d, relay_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        _s.udp().unwrap().client_addr(),
        Some(attacker.local_addr().unwrap()),
        "payload must not be able to influence client_addr"
    );
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 4. Quantify the amplification the relay can be driven into.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn amplification_factor_from_a_minimal_request() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let rdp_task = tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let (len, peer) = rdp.recv_from(&mut buf).await.unwrap();
        // Reply with the largest datagram the relay will carry.
        let big = vec![0xEEu8; shared::crypt::consts::CRYPT_PACKET_SIZE + 32];
        rdp.send_to(&big, peer).await.unwrap();
        len
    });

    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);

    // Smallest acceptable datagram: 24B header + 1B ciphertext + 16B tag.
    let req = csend.encrypt(&token, b"x").unwrap();
    assert_eq!(req.len(), 41, "minimal request wire size");
    attacker.send_to(&req, relay_addr).await.unwrap();
    let forwarded_len = timeout(T, rdp_task).await.unwrap().unwrap();
    assert_eq!(forwarded_len, 1, "relay must forward exactly the payload");

    let mut buf = [0u8; 4096];
    let (len, _) = timeout(T, attacker.recv_from(&mut buf))
        .await
        .expect("no reflected reply")
        .unwrap();
    let factor = len as f64 / req.len() as f64;
    eprintln!(
        "UDP AMPLIFICATION: {}B request -> {}B response ({:.1}x)",
        req.len(),
        len,
        factor
    );
    assert_eq!(len, 24 + MAX_DATAGRAM_PAYLOAD + 16);
    assert!(factor > 30.0, "expected a >30x request/response ratio");
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 5. Unknown-token flood must not disturb a registered leg.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn unknown_token_flood_does_not_evict_a_live_token() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);

    let flooder = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..5000 {
        let t = random_token();
        let mut d = vec![0xAAu8; 64];
        d[..TOKEN_LENGTH].copy_from_slice(&t);
        let _ = flooder.send_to(&d, relay_addr).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        relay
            .sessions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&token),
        "the live token must survive an unknown-token flood"
    );

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);
    let d = csend.encrypt(&token, b"survived").unwrap();
    a.send_to(&d, relay_addr).await.unwrap();
    let mut buf = [0u8; 64];
    let (len, _) = timeout(T, rdp.recv_from(&mut buf))
        .await
        .expect("leg broke after the flood")
        .unwrap();
    assert_eq!(&buf[..len], b"survived");
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 6. The UDP leg must only ever reach remotes[0] (no channel confusion).
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn udp_leg_reaches_only_remotes_index_zero() {
    let rdp0 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a0 = rdp0.local_addr().unwrap();
    let a1 = rdp1.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![a0.to_string(), a1.to_string()], 7).await;
    relay.register(&_s);

    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);
    let d = csend.encrypt(&token, b"to-zero").unwrap();
    c.send_to(&d, relay_addr).await.unwrap();

    let mut buf = [0u8; 64];
    let (len, _) = timeout(T, rdp0.recv_from(&mut buf))
        .await
        .expect("remotes[0] must receive the datagram")
        .unwrap();
    assert_eq!(&buf[..len], b"to-zero");
    assert!(
        timeout(QUIET, rdp1.recv_from(&mut buf)).await.is_err(),
        "remotes[1] must never receive UDP traffic"
    );
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 7. Two sessions must be fully isolated.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn two_sessions_are_isolated() {
    let rdp_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ta = rdp_a.local_addr().unwrap();
    let tb = rdp_b.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (sa, tok_a, sec_a, tic_a) = udp_session(vec![ta.to_string()], 7).await;
    let (sb, _tok_b, sec_b, tic_b) = udp_session(vec![tb.to_string()], 11).await;
    relay.register(&sa);
    relay.register(&sb);

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut asend, _) = launcher(&sec_a, &tic_a);
    let (mut bsend, _) = launcher(&sec_b, &tic_b);
    // Cross-feeding session A's token with session B's key must fail.
    let wrong = bsend.encrypt(&tok_a, b"cross").unwrap();
    client.send_to(&wrong, relay_addr).await.unwrap();
    // Real A traffic still works.
    let da = asend.encrypt(&tok_a, b"from-a").unwrap();
    client.send_to(&da, relay_addr).await.unwrap();

    let mut buf = [0u8; 64];
    let (len, _) = timeout(T, rdp_a.recv_from(&mut buf))
        .await
        .expect("A must receive its datagram")
        .unwrap();
    assert_eq!(&buf[..len], b"from-a");
    assert!(
        timeout(QUIET, rdp_b.recv_from(&mut buf)).await.is_err(),
        "B's remote must not see A's traffic"
    );
    assert!(
        sb.udp().unwrap().client_addr().is_none(),
        "B's return address must stay unset"
    );
    let _ = sa;
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 8. Payload size boundaries through the relay.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn relay_forwards_min_and_max_payloads_verbatim() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);

    let max = vec![0x5Au8; MAX_DATAGRAM_PAYLOAD];
    let dmax = csend.encrypt(&token, &max).unwrap();
    c.send_to(&dmax, relay_addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let (len, _) = timeout(T, rdp.recv_from(&mut buf))
        .await
        .expect("max payload must be forwarded")
        .unwrap();
    assert_eq!(len, MAX_DATAGRAM_PAYLOAD);
    assert_eq!(&buf[..len], &max[..]);
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 9. Reaper vs. in-flight return task (document the overshoot window).
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn return_task_overshoots_the_reap_by_up_to_its_alive_check() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);

    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, mut crecv) = launcher(&secret, &ticket);
    let d = csend.encrypt(&token, b"prime").unwrap();
    c.send_to(&d, relay_addr).await.unwrap();
    let mut buf = [0u8; 4096];
    let _ = timeout(T, rdp.recv_from(&mut buf)).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Reap the leg out from under the return task, then let the remote speak.
    let remote_local = _s
        .udp()
        .unwrap()
        .remote_socket()
        .unwrap()
        .local_addr()
        .unwrap();
    let _ = _s.clear_udp();
    rdp.send_to(b"late-reply", remote_local).await.unwrap();
    let (len, _) = timeout(Duration::from_millis(800), c.recv_from(&mut buf))
        .await
        .expect("return task must still be alive right after the reap")
        .unwrap();
    let late = crecv.decrypt(&token, &buf[..len]).unwrap();
    assert_eq!(late.as_deref(), Some(b"late-reply".as_slice()));
    eprintln!("return-task overshoot after reap: still forwarding (within alive-check window)");
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 10. The return path is not pinned to the first source: ANY source that can
//     present a valid (token, AEAD) pair becomes the return target. In the
//     real world the source address is attacker-chosen (UDP has no handshake
//     and source spoofing is free), which is the precondition for using the
//     relay as a reflector.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn any_authenticating_source_can_repoint_the_return_path() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);
    let (mut csend, mut crecv) = launcher(&secret, &ticket);

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 4096];

    // Source A primes the leg.
    let d = csend.encrypt(&token, b"from-a").unwrap();
    a.send_to(&d, relay_addr).await.unwrap();
    let _ = timeout(T, rdp.recv_from(&mut buf)).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        _s.udp().unwrap().client_addr(),
        Some(a.local_addr().unwrap())
    );
    let remote_local = _s
        .udp()
        .unwrap()
        .remote_socket()
        .unwrap()
        .local_addr()
        .unwrap();

    // Source B (a different address) re-points it with its own valid datagram.
    let d = csend.encrypt(&token, b"from-b").unwrap();
    b.send_to(&d, relay_addr).await.unwrap();
    let _ = timeout(T, rdp.recv_from(&mut buf)).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(
        _s.udp().unwrap().client_addr(),
        Some(b.local_addr().unwrap()),
        "the most recent authenticating source owns the return path"
    );

    // The host's next reply is delivered to B, and never to A.
    rdp.send_to(b"reply-for-b", remote_local).await.unwrap();
    let (len, _) = timeout(T, b.recv_from(&mut buf))
        .await
        .expect("B must receive the reply after re-pointing")
        .unwrap();
    assert_eq!(
        crecv.decrypt(&token, &buf[..len]).unwrap().as_deref(),
        Some(b"reply-for-b".as_slice())
    );
    assert!(
        timeout(QUIET, a.recv_from(&mut buf)).await.is_err(),
        "A must stop receiving once B re-points the leg"
    );
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 11. The single relay task performs remote-leg creation INLINE. When the
//     remote is a name that cannot be resolved, the lookup (a blocking
//     getaddrinfo awaited on the shared loop) is retried on *every*
//     datagram and stalls the relay for every other session.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn a_failing_remote_lookup_stalls_the_shared_relay_loop() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;

    // Healthy session, and a session whose remote name never resolves. The
    // name is unique per run: resolvers negative-cache NXDOMAIN, and a cached
    // answer would hide the cost this probe is about.
    let uniq = format!(
        "udp-audit-{}.invalid",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let (_ok, ok_token, ok_secret, ok_ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_ok);
    let (bad, bad_token, bad_secret, bad_ticket) =
        udp_session(vec![format!("{uniq}:3389")], 8).await;
    relay.register(&bad);

    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut osend, _) = launcher(&ok_secret, &ok_ticket);
    let (mut bsend, _) = launcher(&bad_secret, &bad_ticket);
    let mut buf = [0u8; 4096];

    // Baseline: healthy session alone, right after its leg exists.
    let d = osend.encrypt(&ok_token, b"prime").unwrap();
    c.send_to(&d, relay_addr).await.unwrap();
    let _ = timeout(T, rdp.recv_from(&mut buf)).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    async fn latency_to(c: &UdpSocket, rdp: &UdpSocket, d: &[u8], relay: SocketAddr) -> Duration {
        let t0 = std::time::Instant::now();
        c.send_to(d, relay).await.unwrap();
        let mut b = [0u8; 4096];
        let _ = timeout(T, rdp.recv_from(&mut b)).await.unwrap().unwrap();
        t0.elapsed()
    }

    let mut base_min = Duration::from_secs(9);
    for _ in 0..5 {
        let d = osend.encrypt(&ok_token, b"base").unwrap();
        let l = latency_to(&c, &rdp, &d, relay_addr).await;
        base_min = base_min.min(l);
    }

    // Queue a datagram for the never-resolving session, then the healthy one:
    // the healthy datagram has to wait behind the lookup performed inline on
    // the shared relay loop.
    let mut worst = Duration::from_millis(0);
    let mut first = None;
    let mut all = Vec::new();
    for i in 0..5 {
        let bad_d = bsend.encrypt(&bad_token, b"boom").unwrap();
        c.send_to(&bad_d, relay_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(3)).await;
        let d = osend.encrypt(&ok_token, b"after").unwrap();
        let l = latency_to(&c, &rdp, &d, relay_addr).await;
        if i == 0 {
            first = Some(l);
        }
        worst = worst.max(l);
        all.push(l);
    }
    eprintln!(
        "shared-loop head-of-line: baseline_best={:?}, first_lookup={:?}, worst={:?}, all={:?}",
        base_min,
        first.unwrap_or_default(),
        worst,
        all
    );
    // Relative, so a fast/slow resolver changes the magnitude but not the
    // conclusion: the healthy session waits for the other session's lookup.
    assert!(
        first.unwrap_or_default() > base_min * 3,
        "expected the inline failed lookup to delay a healthy session's \
         datagram (baseline {:?}, first {:?})",
        base_min,
        first.unwrap_or_default()
    );

    // Control: a remote that is a valid *address* (nothing listening) still
    // connects instantly, so there is no DNS and no per-datagram retry. The
    // healthy session must not be delayed by it.
    let (ctrl, ctrl_token, ctrl_secret, ctrl_ticket) =
        udp_session(vec!["127.0.0.1:9".to_string()], 9).await;
    relay.register(&ctrl);
    let (mut ctl_send, _) = launcher(&ctrl_secret, &ctrl_ticket);
    let mut ctrl_worst = Duration::from_millis(0);
    for _ in 0..5 {
        let cd = ctl_send.encrypt(&ctrl_token, b"x").unwrap();
        c.send_to(&cd, relay_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(3)).await;
        let d = osend.encrypt(&ok_token, b"after2").unwrap();
        let l = latency_to(&c, &rdp, &d, relay_addr).await;
        ctrl_worst = ctrl_worst.max(l);
    }
    eprintln!(
        "control (unreachable IP remote, no DNS): worst={:?}",
        ctrl_worst
    );
    // The control is informational: with a fast local resolver the lookup cost
    // is the same order as other setup work, so it is printed, not asserted.
    // The claim that matters is the relative one above (the healthy session
    // waits behind the other session's lookup).
    assert!(ctrl_worst < Duration::from_millis(50));
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 12. Loop escalation: aim the return path at the relay's own socket (the
//     strongest self-reflection a spoofing client could arrange) and confirm
//     the relay's own output cannot re-enter the shared socket as a fresh
//     client datagram, so no self-sustaining packet loop is possible.
//
//     Note: `client_addr` is re-derived from the wire source on every
//     authenticated datagram, so overriding it and then sending again is
//     pointless (the send re-points it at the real source). To keep the
//     override in place the remote is made to speak instead.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn return_path_aimed_at_the_relay_is_rejected_and_cannot_loop() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    let (_s, token, secret, ticket) = udp_session(vec![rdp_addr.to_string()], 7).await;
    relay.register(&_s);
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);
    let mut buf = [0u8; 4096];

    // Build the leg with a genuine datagram from the client.
    let d = csend.encrypt(&token, b"kick").unwrap();
    c.send_to(&d, relay_addr).await.unwrap();
    let _ = timeout(T, rdp.recv_from(&mut buf)).await.unwrap().unwrap();
    let remote_local = _s
        .udp()
        .unwrap()
        .remote_socket()
        .unwrap()
        .local_addr()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Aim the return path at the relay's own listening socket, then make the
    // remote speak (no client datagram, so the override survives).
    _s.udp().unwrap().set_client_addr(relay_addr);
    let before_recv = relay.counters.received.load(Ordering::Relaxed);
    for _ in 0..25 {
        rdp.send_to(b"poke", remote_local).await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let received = relay.counters.received.load(Ordering::Relaxed);
    let sent = relay.counters.sent.load(Ordering::Relaxed);
    let auth_fail = relay.counters.auth_fail.load(Ordering::Relaxed);
    let forwarded = relay.counters.forwarded.load(Ordering::Relaxed);
    eprintln!(
        "self-reflection loop probe: received={received} (was {before_recv}) sent={sent} auth_fail={auth_fail} forwarded={forwarded} unknown={} replay={}",
        relay.counters.discarded_unknown.load(Ordering::Relaxed),
        relay.counters.discarded_replay.load(Ordering::Relaxed),
    );

    // Each reflected reply comes back onto the shared socket encrypted with the
    // s2c key; the inbound decrypt uses the c2s key, so it is rejected and the
    // loop cannot even start.
    assert!(
        received >= 26,
        "reflected replies must come back onto the shared socket (received={received})"
    );
    assert!(
        auth_fail >= 25,
        "every reflected reply must be rejected, auth_fail={auth_fail}"
    );
    assert_eq!(
        forwarded, 1,
        "a rejected reply must never be forwarded back to the remote (loop!)"
    );
    assert!(
        sent >= 25,
        "the return path must have re-emitted (sent={sent})"
    );
    stop.trigger();
}

// ---------------------------------------------------------------------------
// 13. Regression probe for the source-IP pinning fix (see the reflection
//     report). On the current code `client_addr` is taken from the wire source
//     with no check against the tunnel peer, so a spoofed source becomes the
//     return target. Ignored until the fix is applied.
// ---------------------------------------------------------------------------

#[serial_test::serial(config, manager)]
#[tokio::test]
#[ignore = "fails on the current code: client_addr is taken from the wire source without checking it against the TCP peer"]
async fn source_ip_must_match_the_tcp_peer_before_repointing_the_return_path() {
    let rdp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rdp_addr = rdp.local_addr().unwrap();
    let (relay, stop, relay_addr) = host_relay().await;
    // The tunnel peer is 10.9.9.9, but the datagram arrives from 127.0.0.1.
    let (_s, token, secret, ticket) =
        udp_session_full(vec![rdp_addr.to_string()], 7, "10.9.9.9:1234").await;
    relay.register(&_s);
    let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut csend, _) = launcher(&secret, &ticket);
    let d = csend.encrypt(&token, b"spoofed-source").unwrap();
    c.send_to(&d, relay_addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        _s.udp().unwrap().client_addr(),
        None,
        "a source whose ip differs from the tunnel peer must not become the return target"
    );
    stop.trigger();
}
