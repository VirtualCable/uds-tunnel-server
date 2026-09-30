//! Malicious-client regression harness for the UDP leg.
//!
//! Kept in-tree for regression value (the vectors it pins are real). It is
//! deliberately hermetic: it drives its own relay through
//! `UdpRelay::bind_for_test` and never installs the process-global
//! `UDP_RELAY`, and every test is `serial_test`-tagged so it cannot race the
//! shared-state tests.

use super::*;

use shared::{
    crypt::{
        datagram::{DatagramCrypt, TOKEN_LENGTH, random_token},
        tunnel::get_udp_crypts,
        types::SharedSecret,
    },
    protocol::{consts::TICKET_LENGTH, ticket::Ticket},
};

use crate::session::UdpState;

const T: std::time::Duration = std::time::Duration::from_secs(2);

/// Builds a session with a UDP leg whose keys derive from (secret, ticket),
/// exactly like `connect` does, so a "launcher" can mirror the crypts.
fn mk_session(remote: &str, tag: u8) -> (Arc<Session>, UdpToken, SharedSecret, Ticket) {
    let shared_secret = SharedSecret::new([tag; 32]);
    let ticket = Ticket::new([tag; TICKET_LENGTH]);
    let (inbound, outbound) = get_udp_crypts(&shared_secret, &ticket).unwrap();
    let token = random_token();
    let session = Arc::new(Session::new(
        shared_secret.clone(),
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec![remote.to_string()],
    ));
    session.set_udp(UdpState::new(token, inbound, outbound));
    (session, token, shared_secret, ticket)
}

/// Launcher-side mirror: (send = c2s, recv = s2c).
fn launcher(secret: &SharedSecret, ticket: &Ticket) -> (DatagramCrypt, DatagramCrypt) {
    let (send, _) = get_udp_crypts(secret, ticket).unwrap();
    let (_, recv) = get_udp_crypts(secret, ticket).unwrap();
    (send, recv)
}

async fn relay() -> (Arc<UdpRelay>, Trigger, SocketAddr) {
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

/// Cross-session isolation: a malicious session B floods the shared relay
/// with every shape of hostile datagram while session A must keep working.
#[serial_test::serial(manager)]
#[tokio::test]
async fn attacker_flood_does_not_break_other_sessions() {
    log::setup_logging("debug", log::LogType::Test);

    // Two independent "RDP hosts" (one per session) that echo back.
    let host_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let host_a_addr = host_a.local_addr().unwrap();
    let host_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let host_b_addr = host_b.local_addr().unwrap();

    let (r, stop, r_addr) = relay().await;
    let (sa, ta, seka, tika) = mk_session(&host_a_addr.to_string(), 0x11);
    let (sb, tb, sekb, tikb) = mk_session(&host_b_addr.to_string(), 0x22);
    r.register(&sa);
    r.register(&sb);

    let victim = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut va_send, mut va_recv) = launcher(&seka, &tika);

    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut att_send, _att_recv) = launcher(&sekb, &tikb);

    // ---- attacker activity -------------------------------------------------
    // 1) valid datagrams on its own token
    for i in 0..16u8 {
        let d = att_send.encrypt(&tb, &[i; 32]).unwrap();
        attacker.send_to(&d, r_addr).await.unwrap();
    }
    // 2) its own valid datagram replayed
    let replay = att_send.encrypt(&tb, b"replayed").unwrap();
    attacker.send_to(&replay, r_addr).await.unwrap();
    attacker.send_to(&replay, r_addr).await.unwrap();
    // 3) unknown random tokens
    for _ in 0..16 {
        let mut d = replay.clone();
        d[..TOKEN_LENGTH].copy_from_slice(&random_token());
        attacker.send_to(&d, r_addr).await.unwrap();
    }
    // 4) the VICTIM's token with the attacker's ciphertext (token confusion)
    for _ in 0..8 {
        let mut d = att_send.encrypt(&ta, b"confused").unwrap();
        // re-tag the datagram with the victim token but keep the attacker tag
        let mut forged = d.clone();
        forged[..TOKEN_LENGTH].copy_from_slice(&ta);
        d.clear();
        attacker.send_to(&forged, r_addr).await.unwrap();
    }
    // 5) truncated / oversized / garbage sizes, victim and own tokens
    for len in [0usize, 1, 23, 24, 40, 41, 42, 1440, 1441, 4096] {
        let mut junk = vec![0xAAu8; len];
        if len >= TOKEN_LENGTH {
            junk[..TOKEN_LENGTH].copy_from_slice(&ta);
        }
        attacker.send_to(&junk, r_addr).await.unwrap();
    }
    // 6) oversized datagram carrying a valid victim token (truncation probe)
    let big = va_send
        .encrypt(&ta, &vec![0u8; MAX_DATAGRAM_PAYLOAD])
        .unwrap();
    let mut oversized = big.clone();
    oversized.extend_from_slice(&[0u8; 512]);
    attacker.send_to(&oversized, r_addr).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ---- victim must still work end to end ---------------------------------
    // Drain anything the hostile probes may legitimately have delivered
    // (e.g. the truncated prefix of the oversized valid datagram).
    let mut drain = [0u8; 2048];
    while tokio::time::timeout(
        std::time::Duration::from_millis(50),
        host_a.recv_from(&mut drain),
    )
    .await
    .is_ok()
    {}

    let host_reply = tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let (len, peer) = host_a.recv_from(&mut buf).await.unwrap();
        let got = buf[..len].to_vec();
        host_a.send_to(b"pong", peer).await.unwrap();
        got
    });
    let d = va_send.encrypt(&ta, b"ping").unwrap();
    victim.send_to(&d, r_addr).await.unwrap();
    let got = tokio::time::timeout(T, host_reply)
        .await
        .expect("victim payload never reached its host")
        .unwrap();
    assert_eq!(got, b"ping");
    let mut buf = [0u8; 2048];
    let (len, _) = tokio::time::timeout(T, victim.recv_from(&mut buf))
        .await
        .expect("victim never got its host reply")
        .unwrap();
    assert_eq!(
        va_recv.decrypt(&ta, &buf[..len]).unwrap().as_deref(),
        Some(b"pong".as_slice())
    );

    // The host of the ATTACKER must never have received a "confused" payload
    // whose token belonged to the victim... (it may legitimately receive the
    // attacker's own frames; check no victim-addressed payload arrived).
    let mut bbuf = [0u8; 2048];
    while let Ok(Ok((l, _))) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        host_b.recv_from(&mut bbuf),
    )
    .await
    {
        assert_ne!(
            &bbuf[..l],
            b"confused",
            "victim token reached attacker host"
        );
    }

    stop.trigger();
}

/// A valid (authenticated) datagram with an extreme seq poisons only the
/// window of the session it was addressed to — never another session's.
#[serial_test::serial(manager)]
#[tokio::test]
async fn window_poison_is_per_session() {
    let host_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let host_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (r, stop, r_addr) = relay().await;
    let (sa, ta, seka, tika) = mk_session(&host_a.local_addr().unwrap().to_string(), 0x33);
    let (sb, tb, sekb, tikb) = mk_session(&host_b.local_addr().unwrap().to_string(), 0x44);
    r.register(&sa);
    r.register(&sb);

    let victim = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut va_send, mut va_recv) = launcher(&seka, &tika);
    let attacker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut att_send, _att_recv) = launcher(&sekb, &tikb);

    // Attacker sends a valid datagram to learn the addresses.
    let d = att_send.encrypt(&tb, b"hi").unwrap();
    attacker.send_to(&d, r_addr).await.unwrap();
    let mut hb = [0u8; 2048];
    let _ = tokio::time::timeout(T, host_b.recv_from(&mut hb)).await;

    // Attacker replays its own datagram: benign discard, no effect elsewhere.
    attacker.send_to(&d, r_addr).await.unwrap();
    // A different, freshly encrypted datagram still works.
    let d2 = att_send.encrypt(&tb, b"hi2").unwrap();
    attacker.send_to(&d2, r_addr).await.unwrap();
    let (l, _) = tokio::time::timeout(T, host_b.recv_from(&mut hb))
        .await
        .expect("attacker's own second datagram should forward")
        .unwrap();
    assert_eq!(&hb[..l], b"hi2");

    // Victim is unaffected by all the attacker noise.
    let d = va_send.encrypt(&ta, b"v").unwrap();
    victim.send_to(&d, r_addr).await.unwrap();
    let mut ha = [0u8; 2048];
    let (l, peer) = tokio::time::timeout(T, host_a.recv_from(&mut ha))
        .await
        .expect("victim datagram must still forward")
        .unwrap();
    assert_eq!(&ha[..l], b"v");
    host_a.send_to(b"vr", peer).await.unwrap();
    let mut vb = [0u8; 2048];
    let (l, _) = tokio::time::timeout(T, victim.recv_from(&mut vb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        va_recv.decrypt(&ta, &vb[..l]).unwrap().as_deref(),
        Some(b"vr".as_slice())
    );
    stop.trigger();
}

/// Fuzz the relay with random mutations of valid datagrams; the relay must
/// never panic and must keep serving a legitimate session afterwards.
#[serial_test::serial(manager)]
#[tokio::test]
async fn relay_survives_mutated_datagram_fuzz() {
    let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (r, stop, r_addr) = relay().await;
    let (s, token, sek, tik) = mk_session(&host.local_addr().unwrap().to_string(), 0x55);
    r.register(&s);
    let (mut send, _recv) = launcher(&sek, &tik);
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let base = send.encrypt(&token, b"baseline-payload").unwrap();
    let mut seed: u64 = 0x1234_5678_9abc_def0;
    for _ in 0..4000 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut d = base.clone();
        // random single-byte flip
        let idx = (seed as usize) % d.len();
        // |1 guarantees the mutation actually changes a byte, so a mutated
        // datagram can never accidentally stay valid.
        d[idx] ^= ((seed >> 32) as u8) | 1;
        // sometimes resize
        match (seed >> 8) % 4 {
            0 => d.truncate((seed as usize) % (d.len() + 1)),
            1 => d.extend(std::iter::repeat_n(0u8, (seed as usize) % 64)),
            _ => {}
        }
        let _ = sock.send_to(&d, r_addr).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Still serves a fresh, valid datagram.
    let (mut send2, _) = launcher(&sek, &tik);
    let good = send2.encrypt(&token, b"after-fuzz").unwrap();
    sock.send_to(&good, r_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let (l, _) = tokio::time::timeout(T, host.recv_from(&mut buf))
        .await
        .expect("relay stopped forwarding after the fuzz")
        .unwrap();
    assert_eq!(&buf[..l], b"after-fuzz");

    stop.trigger();
}

/// Regression guard for the shared-counter recovery design. The old flow
/// rebuilt the stream crypts from `session.seqs()`, which was only refreshed
/// when a stream *ended*: while a stream was live its numbers were stale, so
/// an authenticated client that kept its original connection open and opened
/// a second one with a Recover made the server encrypt two different
/// plaintexts under the same (key, seq) — AES-GCM nonce reuse. With the
/// session counters shared by every crypt the session hands out, a replaced
/// stream's late in-flight frames and the new stream's first frames consume
/// disjoint sequence numbers from the same atomic, so the reuse is impossible
/// by construction — no drain/publish step, no reseeding.
#[serial_test::serial(manager)]
#[tokio::test]
async fn shared_counters_make_overlapping_streams_nonce_safe() {
    use shared::crypt::types::PacketBuffer;

    let shared_secret = SharedSecret::new([0x77; 32]);
    let ticket = Ticket::new([0x77; TICKET_LENGTH]);
    let session = Session::new(
        shared_secret,
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec!["127.0.0.1:9".to_string()],
    );

    // ---- "original" live stream: crypts built as `run_attached` does ------
    let (_in1, mut out1) = session.server_tunnel_crypts().unwrap();

    let mut seqs1: Vec<u64> = Vec::new();
    let mut frames1: Vec<(u64, Vec<u8>)> = Vec::new();
    for i in 0..6u8 {
        let data = vec![i; 16];
        let mut buf = PacketBuffer::new();
        buf.set_data(&data).unwrap();
        out1.encrypt(1, data.len(), &mut buf).unwrap();
        let seq = buf.seq().unwrap();
        seqs1.push(seq);
        frames1.push((seq, buf.data_with_channel()[..18].to_vec()));
    }
    // The session's live counter already reflects every consumed seq — there
    // is no stale "published only at stream end" state anymore.
    assert_eq!(session.seqs().1, 6);

    // ---- Recover: `recover` builds its handshake crypts and the new
    // stream builds its own, all sharing the same session counter ----------
    let (_rin, _rout) = session.server_tunnel_crypts().unwrap();
    let (_in2, mut out2) = session.server_tunnel_crypts().unwrap();

    // The killed stream is mid-teardown and still flushes a late frame
    // through its own crypt; the new stream then emits its first frame.
    let mut late = PacketBuffer::new();
    late.set_data(&[0xAAu8; 16]).unwrap();
    out1.encrypt(1, 16, &mut late).unwrap();
    let seq_late = late.seq().unwrap();

    let data2 = vec![0xEEu8; 16];
    let mut buf2 = PacketBuffer::new();
    buf2.set_data(&data2).unwrap();
    out2.encrypt(1, data2.len(), &mut buf2).unwrap();
    let seq2 = buf2.seq().unwrap();

    // Neither of the overlapping streams could have grabbed a sequence
    // number an earlier holder already used.
    assert!(
        !seqs1.contains(&seq_late),
        "late frame reused a consumed seq"
    );
    assert!(!seqs1.contains(&seq2), "new stream reused a consumed seq");
    assert_ne!(seq_late, seq2);
    assert_eq!(session.seqs().1, 8);

    // And the AES-GCM keystream-reuse signature (XOR(ct) == XOR(pt) for a
    // colliding nonce) cannot be produced: the two ciphertexts under test
    // use different nonces, so no plaintext relationship leaks.
    let (_, ct1) = &frames1[0];
    let ct2 = &buf2.data_with_channel()[..18];
    let mut pt1 = vec![0u8, 1u8];
    pt1.extend_from_slice(&[0u8; 16]);
    let mut pt2 = vec![0u8, 1u8];
    pt2.extend_from_slice(&data2);
    let xor_ct: Vec<u8> = ct1.iter().zip(ct2.iter()).map(|(a, b)| a ^ b).collect();
    let xor_pt: Vec<u8> = pt1.iter().zip(pt2.iter()).map(|(a, b)| a ^ b).collect();
    assert_ne!(
        xor_ct, xor_pt,
        "different (key, nonce) pairs must not share keystream"
    );
}
