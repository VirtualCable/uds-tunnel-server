//! Regression tests for the session/stream coordination in recovery and
//! attach. Threat model: a party that knows a captured `equiv_session_id`
//! but holds NO key, plus the authenticated client.
//!
//! These are hermetic: they build a `Session` directly (known secret/ticket),
//! mint its equiv id, register it in the `SessionManager`, and drive the real
//! `recover()` handler over an in-memory duplex leg. No broker/network needed.
//!
//! History: a security review detected that the Recover handler consumed the
//! victim's retransmission window and held the attach lock before the AEAD
//! ticket confirm authenticated the peer, and that an attach queued on the
//! proxy control channel could strand `start_server()` forever when the proxy
//! task exited. Each test below pins the fixed behaviour: it fails against
//! the pre-fix code and passes against the current one.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use shared::{
    crypt::{
        Crypt,
        tunnel::derive_tunnel_material,
        types::{PacketBuffer, SharedSecret},
    },
    protocol::{PayloadWithChannel, consts::TICKET_LENGTH, ticket::Ticket},
    system::trigger::Trigger,
};

use crate::session::{Session, SessionManager};

use super::recover::recover;

/// Address every `hermetic_session` registers with, and the leg every
/// pre-existing window test drives recovery from. B3 tests pass a different
/// `ip` to observe whether the recovering peer's address gets adopted.
const ORIG_IP: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
    0,
);
/// The address a client that moved networks (VPN re-bind, NAT rebinding,
/// Wi-Fi switch) recovers from.
const NEW_IP: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 7)),
    31337,
);

/// A registered session with a known secret/ticket and a minted equiv id, so
/// a test can act as the launcher (mirror crypts) or as a key-less attacker
/// that only knows the equiv id.
async fn hermetic_session(tag: u8) -> (Arc<Session>, Ticket, SharedSecret, Ticket) {
    let shared_secret = SharedSecret::new([tag; 32]);
    let ticket = Ticket::new([b'A' + (tag % 26); TICKET_LENGTH]);
    let equiv = Ticket::new([b'a' + (tag % 26); TICKET_LENGTH]);

    let session = Session::new(
        shared_secret.clone(),
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec!["127.0.0.1:3389".to_string()],
    );
    session.set_current_equiv_id(Some(equiv));
    let session = SessionManager::get_instance()
        .add_session(session)
        .expect("add_session");
    (session, equiv, shared_secret, ticket)
}

fn push_buffered(session: &Session, seq: u64) {
    session
        .recovery_buffer()
        .lock()
        .push(seq, PayloadWithChannel::new(1, &[0xAAu8; 90]))
        .expect("buffer push");
}

/// Advance the session's shared outbound counter without touching the
/// recovery buffer — exactly what a recovery handshake's `OpenResponse`
/// does in production. Keeps `session.seqs().1` consistent with a buffer
/// window the test stamps manually.
fn advance_outbound(session: &Session, times: u64) {
    let (_in, mut out) = session.server_tunnel_crypts().expect("crypts");
    let mut buf = PacketBuffer::new();
    buf.set_data(b"advance").expect("set_data");
    for _ in 0..times {
        out.encrypt(0, 7, &mut buf).expect("encrypt");
    }
}

/// Drive `recover()` with a real AEAD ticket confirm, then cut the leg so a
/// passed window validation can only fail later on the `OpenResponse` write.
/// This pins the window-check outcome deterministically: an error whose
/// message contains `Invalid recovery sequence` means the recovery was
/// refused at the window check; anything else (the write error) means the
/// validation accepted it.
async fn recover_with_valid_confirm(
    equiv: &Ticket,
    secret: &SharedSecret,
    ticket: &Ticket,
    in_seqs: (u64, u64),
    ip: SocketAddr,
) -> anyhow::Result<()> {
    let material = derive_tunnel_material(secret, ticket)?;
    let (client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let equiv_owned = *equiv;
    let handle = tokio::spawn(async move { recover(r, w, &equiv_owned, in_seqs, ip).await });

    let (cr, mut cw) = tokio::io::split(client);
    let mut confirm = PacketBuffer::new();
    confirm.set_data(equiv.as_ref())?;
    Crypt::new(&material.key_receive, 0).encrypt(0, equiv.as_ref().len(), &mut confirm)?;
    confirm.write(&mut cw).await?;
    drop((cr, cw)); // cut the leg: the handshake can go no further than the response write

    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("recover must not hang after a valid confirm and a cut leg")
        .expect("recover task panicked")
}

/// A leg that dropped after the previous leg's `OpenResponse` and before any
/// data frame was buffered: the retransmission window is empty while the
/// launcher declares the last seq the session encrypted. The declaration is
/// legitimate — nothing is pending retransmission — and must not be refused
/// just because `tail_seq()` is zero.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_accepts_empty_buffer_after_last_encrypted_frame() {
    let (session, equiv, secret, ticket) = hermetic_session(5).await;
    advance_outbound(&session, 3); // the wire reached seq 3

    let res = recover_with_valid_confirm(&equiv, &secret, &ticket, (4, 1), ORIG_IP).await;
    if let Err(e) = res {
        assert!(
            !e.to_string().contains("Invalid recovery sequence"),
            "legitimate empty-buffer recovery refused: {e}"
        );
    }
    SessionManager::get_instance().remove_session(session.id());
}

/// `requested == head - 1`: the launcher acknowledged none of the buffered
/// window, which means "re-send all of it". The window must survive the
/// handshake untouched for the new stream to replay it.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_keeps_whole_window_when_launcher_acked_nothing() {
    let (session, equiv, secret, ticket) = hermetic_session(6).await;
    push_buffered(&session, 2);
    advance_outbound(&session, 2); // tail == last sent: production-consistent

    let res = recover_with_valid_confirm(&equiv, &secret, &ticket, (2, 1), ORIG_IP).await;
    if let Err(e) = res {
        assert!(
            !e.to_string().contains("Invalid recovery sequence"),
            "re-send-all recovery refused: {e}"
        );
    }
    assert_eq!(
        session.recovery_buffer().lock().len(),
        1,
        "the window due for retransmission was destroyed instead of replayed"
    );
    SessionManager::get_instance().remove_session(session.id());
}

/// A declaration above everything the session ever encrypted cannot be
/// satisfied by any retransmission: refuse it and leave the window intact.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_refuses_declaration_above_last_encrypted_frame() {
    let (session, equiv, secret, ticket) = hermetic_session(7).await;
    push_buffered(&session, 2);
    advance_outbound(&session, 2);

    let err = recover_with_valid_confirm(&equiv, &secret, &ticket, (5, 1), ORIG_IP)
        .await
        .expect_err("a fabricated declaration must be refused");
    assert!(
        err.to_string().contains("beyond last sent seq"),
        "expected the last-sent refusal, got: {err}"
    );
    assert_eq!(
        session.recovery_buffer().lock().len(),
        1,
        "a refused recovery must leave the window intact"
    );
    SessionManager::get_instance().remove_session(session.id());
}

/// A declaration below the retained window whose gap frames were neither
/// buffered nor acknowledged is an eviction hole: no contiguous
/// retransmission can satisfy it, so refuse it.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_refuses_declaration_below_evicted_gap() {
    let (session, equiv, secret, ticket) = hermetic_session(8).await;
    for seq in 5..8 {
        push_buffered(&session, seq); // head 5; 1..4 were evicted
    }
    advance_outbound(&session, 7);

    let err = recover_with_valid_confirm(&equiv, &secret, &ticket, (3, 1), ORIG_IP)
        .await
        .expect_err("an evicted-gap declaration must be refused");
    assert!(
        err.to_string().contains("below recovery buffer window"),
        "expected the evicted-gap refusal, got: {err}"
    );
    assert_eq!(
        session.recovery_buffer().lock().len(),
        3,
        "a refused recovery must leave the window intact"
    );
    SessionManager::get_instance().remove_session(session.id());
}

/// An unauthenticated Recover (equiv id known, no key) must not mutate the
/// victim's recovery buffer nor hold the attach lock: the window check and
/// `skip` run only AFTER the AEAD ticket-confirm authenticates the peer, so a
/// key-less peer can neither destroy the retransmission window nor wedge the
/// session. A confirm timeout alone must not kill the session either.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn unauthenticated_recover_does_not_mutate_buffer_before_authentication() -> anyhow::Result<()>
{
    let (session, equiv, _secret, _ticket) = hermetic_session(1).await;
    push_buffered(&session, 2);
    assert_eq!(session.recovery_buffer().lock().len(), 1);

    // No key: send Recover with an in-window in_seq (requested = 3 - 1 = 2,
    // the only buffered seq) and then go silent (never send the confirm).
    let (client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = tokio::spawn(async move { recover(r, w, &equiv, (3, 1), ip).await });

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !handle.is_finished(),
        "recover must still be blocked on the (unauthenticated) confirm read"
    );
    // FIXED BEHAVIOUR PIN: the buffer is still intact while the peer has sent
    // no AEAD frame at all (it cannot: it has no key). Pre-fix this was
    // already empty here.
    assert_eq!(
        session.recovery_buffer().lock().len(),
        1,
        "recovery buffer was mutated before the AEAD confirm authenticated the peer"
    );

    let res = tokio::time::timeout(Duration::from_secs(3), handle).await??;
    let err = res.expect_err("the silent recover must fail on its confirm timeout");
    assert!(
        err.to_string()
            .contains("Timeout waiting for recover session id"),
        "expected the confirm timeout, got: {err}"
    );
    // A mere timeout (a slow client) must NOT kill the session either.
    assert!(
        SessionManager::get_instance()
            .get_equiv_session(&equiv)
            .is_some(),
        "the timed-out recover must leave the session intact"
    );
    drop(client);
    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}

/// An unauthenticated Recover must not hold the per-session attach lock during
/// its 1 s confirm-read timeout, so a key-less Recover-and-silent attempt
/// cannot stall a legitimate attach.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn unauthenticated_recover_does_not_stall_legitimate_attach() -> anyhow::Result<()> {
    let (session, equiv, _secret, _ticket) = hermetic_session(2).await;
    push_buffered(&session, 2);

    let (client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let handle = tokio::spawn(async move { recover(r, w, &equiv, (3, 1), ip).await });

    // The attacker is inside its confirm read.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A legitimate attach takes the very same lock.
    let started = std::time::Instant::now();
    let guard = session.lock_server_attach().await;
    let elapsed = started.elapsed();
    drop(guard);
    // FIXED BEHAVIOUR PIN: the confirm read happens before the attach lock is
    // taken, so `elapsed` is near zero. Pre-fix it was ~1 s (the full confirm
    // timeout) for every silent attempt.
    assert!(
        elapsed < Duration::from_millis(100),
        "legitimate attach was stalled by the silent Recover (elapsed={elapsed:?})"
    );

    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    drop(client);
    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}

/// End-to-end: a key-less attacker's timed-out Recover leaves the
/// retransmission window intact, and the legitimate client's next Recover
/// (same, valid in_seq) still succeeds.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn timed_out_recover_preserves_legitimate_recovery() -> anyhow::Result<()> {
    let (session, equiv, secret, ticket) = hermetic_session(3).await;
    push_buffered(&session, 2);

    // Attacker (equiv id only) times out without touching any state.
    let (client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), recover(r, w, &equiv, (3, 1), ip))
        .await
        .expect("the silent recover must fail on its own timeout, not hang")
        .expect_err("the silent recover must return an error");
    drop(client);
    assert_eq!(
        session.recovery_buffer().lock().len(),
        1,
        "the timed-out attacker must not have drained the window"
    );

    // The legitimate client's Recover: same equiv id, a real AEAD ticket
    // confirm (the attacker could not produce one), valid in_seq. It must
    // authenticate, consume the window, and attach cleanly.
    let material = derive_tunnel_material(&secret, &ticket)?;
    let (mut client2, server2) = tokio::io::duplex(1024);
    let (r2, w2) = tokio::io::split(server2);
    let recover_handle = tokio::spawn(async move { recover(r2, w2, &equiv, (3, 1), ip).await });

    let mut confirm = PacketBuffer::new();
    confirm.set_data(equiv.as_ref())?;
    // Fresh session counters: the confirm is the first inbound frame (seq 1).
    Crypt::new(&material.key_receive, 0).encrypt(0, equiv.as_ref().len(), &mut confirm)?;
    confirm.write(&mut client2).await?;

    let res = tokio::time::timeout(Duration::from_secs(3), recover_handle)
        .await
        .expect("the authenticated recover must complete, not hang")
        .expect("recover task panicked");
    if let Err(e) = res {
        panic!("legitimate recovery must succeed: {e}");
    }
    assert_eq!(
        session.recovery_buffer().lock().len(),
        0,
        "the authenticated recover consumes the window it declared"
    );

    drop(client2);
    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}

/// A session negotiated with `k = 8` must survive a `Recover` under the
/// *same* `k` even if the config changed meanwhile: the handler re-advertises
/// `session.rekey_log2()` (never re-reads the config) and the crypts it
/// rebuilds for the session epoch by the pinned threshold. Threat model for
/// rekeying (docs/rekeying-contract.md §4): if a recover re-read config, a
/// mid-life config flip would silently drift the key schedule and the
/// launcher — still holding the original `k` — would diverge hard.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_reuses_the_persisted_session_k() -> anyhow::Result<()> {
    use shared::crypt::tunnel::get_tunnel_crypts;

    let tag = 11u8;
    let shared_secret = SharedSecret::new([tag; 32]);
    let ticket = Ticket::new([b'K' + (tag % 26); TICKET_LENGTH]);
    let equiv = Ticket::new([b'k' + (tag % 26); TICKET_LENGTH]);

    // Session negotiated with k = 8 (rotates every 256 frames).
    let session = Session::with_rekey_log2(
        shared_secret.clone(),
        ticket,
        Trigger::new(),
        ORIG_IP,
        vec!["127.0.0.1:3389".to_string()],
        None,
        8,
    );
    session.set_current_equiv_id(Some(equiv));
    let session = SessionManager::get_instance()
        .add_session(session)
        .expect("add_session");
    push_buffered(&session, 2);

    // The config now advertises a DIFFERENT threshold: any drift in the
    // recover path would leak it into the handshake or the rebuilt crypts.
    let previous = {
        let cfg = crate::config::get();
        let mut cfg = cfg.write().unwrap_or_else(|e| e.into_inner());
        let previous = cfg.rekey_seq_log2;
        cfg.rekey_seq_log2 = Some(20);
        previous
    };

    let material = derive_tunnel_material(&shared_secret, &ticket)?;

    // Leg + real recover handshake: confirm at seq 1 (epoch 0 under any k,
    // the legacy material), OpenResponse read by the launcher mirror.
    let (mut client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let equiv_owned = equiv;
    let handle = tokio::spawn(async move { recover(r, w, &equiv_owned, (3, 1), ORIG_IP).await });

    let mut confirm = PacketBuffer::new();
    confirm.set_data(equiv.as_ref())?;
    Crypt::new(&material.key_receive, 0).encrypt(0, equiv.as_ref().len(), &mut confirm)?;
    confirm.write(&mut client).await?;

    let mut resp_buf = PacketBuffer::new();
    let (data, _ch) = Crypt::new(&material.key_send, 0)
        .read(&mut client, &mut resp_buf)
        .await?;
    let response = super::types::OpenResponse::try_from(data)?;

    // THE PIN: the advertised threshold is the session's persisted k, not
    // the config's current one.
    assert_eq!(response.rekey_log2, 8, "recover must not renegotiate k");
    assert_eq!(session.rekey_log2(), 8);

    // Rebuilt crypts epoch by the session's k, never the config's. A
    // launcher->server frame deep in epoch 19 (seq 5000 >> 8) must decrypt
    // through the session's inbound...
    let mut launcher_mirror = get_tunnel_crypts(
        &shared_secret,
        &ticket,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(4999)),
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        session.rekey_log2(),
    )?
    .0;
    let mut frame = PacketBuffer::new();
    frame.set_data(b"across-epoch-boundary")?;
    launcher_mirror.encrypt(1, 21, &mut frame)?;
    assert_eq!(frame.seq().unwrap(), 5000);
    let mut session_in = session.server_tunnel_crypts()?.0;
    session_in
        .decrypt(&mut frame)
        .expect("rebuilt session crypt must epoch by the session's k (5000 >> 8 = 19)");
    assert_eq!(frame.data(), b"across-epoch-boundary");
    assert_eq!(session.seqs().0, 5001);

    // ...while a peer that had re-read the config (k = 20: seq 5000 still
    // epoch 0, legacy key) produces a frame the session crypt rejects at
    // the AEAD — the divergence is loud, silent corruption impossible.
    let mut config_drifted = get_tunnel_crypts(
        &shared_secret,
        &ticket,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(5999)),
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        20,
    )?
    .0;
    let mut wrong = PacketBuffer::new();
    wrong.set_data(b"wrong-k")?;
    config_drifted.encrypt(1, 7, &mut wrong)?;
    let err = session
        .server_tunnel_crypts()?
        .0
        .decrypt(&mut wrong)
        .expect_err("a config-drifted peer must not decrypt under the session k");
    assert!(
        err.to_string().contains("decryption failure"),
        "expected AEAD rejection of a drifted-k frame, got: {err}"
    );

    handle.abort();
    drop(client);
    let cfg = crate::config::get();
    cfg.write()
        .unwrap_or_else(|e| e.into_inner())
        .rekey_seq_log2 = previous;
    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}

/// Characterisation (ignored by default): with a shared `seq_in`, `Crypt::decrypt` is check-then-act
/// — it loads `current_seq`, runs the whole AES-GCM verification, and only then
/// `fetch_max`es the counter. Two inbound crypts that share one counter
/// therefore BOTH accept the same frame if they overlap (recover handshake
/// crypt vs the live stream crypt, or a replaced stream vs its replacement).
/// Consequence is a duplicated frame delivered to the proxy, not a crypto
/// break: both holders already possess the per-session key.
///
/// Ignored because it MEASURES a known defect: pinning `doubles > 0` would
/// fail the day the check-then-act becomes atomic (the desirable fix), so it
/// is a characterization probe to run manually (`cargo test -- --ignored`)
/// before and after any anti-replay hardening, not a regression gate.
#[test]
#[ignore]
fn duplicate_accept_is_possible_while_decrypt_is_check_then_act() {
    use shared::crypt::{
        Crypt,
        types::{PacketBuffer, SharedSecret},
    };
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    };

    let key = SharedSecret::new([0x5Au8; 32]);
    let counter = Arc::new(AtomicU64::new(0));

    // A launcher frame at seq 1, encrypted under the same key.
    let mut frame = PacketBuffer::new();
    frame.set_data(b"duplicate").unwrap();
    Crypt::with_counter(&key, Arc::new(AtomicU64::new(0)))
        .encrypt(0, 9, &mut frame)
        .unwrap();
    assert_eq!(frame.seq().unwrap(), 1);

    let iters = 300u32;
    let mut doubles = 0u32;
    for _ in 0..iters {
        counter.store(0, Ordering::SeqCst);
        let barrier = Arc::new(Barrier::new(2));
        let (c1, c2) = (counter.clone(), counter.clone());
        let (k1, k2) = (key.clone(), key.clone());
        let (b1, b2) = (barrier.clone(), barrier.clone());
        let (mut f1, mut f2) = (frame.clone(), frame.clone());
        let t1 = std::thread::spawn(move || {
            let mut c = Crypt::with_counter(&k1, c1);
            b1.wait();
            c.decrypt(&mut f1).is_ok()
        });
        let t2 = std::thread::spawn(move || {
            let mut c = Crypt::with_counter(&k2, c2);
            b2.wait();
            c.decrypt(&mut f2).is_ok()
        });
        if t1.join().unwrap() && t2.join().unwrap() {
            doubles += 1;
        }
    }
    eprintln!("shared-counter duplicate-accept: {doubles}/{iters}");
    assert!(
        doubles > 0,
        "shared-counter decrypt never double-accepted the same frame"
    );
}

/// `Handler::start_server` enqueues `AttachServer { reply }` on the proxy
/// control channel and then awaits `reply`. If the proxy task exits with that
/// command still queued (the `stop`/`ServerStopped` arms are `biased` and run
/// first), the queued command — and the `reply` sender inside it — used to stay
/// alive because the `Session` still owns the channel's sender, so
/// `start_server()` blocked FOREVER.
///
/// The fix: the proxy drains the control queue on every exit path and drops the
/// queued reply senders, so `recv_async` observes the disconnection and
/// `start_server` fails promptly; `Session::start_server` then rolls back
/// `server_running` so the session does not claim a stream that never
/// attached.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn start_server_fails_fast_when_proxy_exits_with_attach_queued() -> anyhow::Result<()> {
    let (session, _equiv, _secret, _ticket) = hermetic_session(7).await;
    // First attach succeeds and gives the proxy something to detach.
    let (_endpoints, _owner) = session.start_server().await?;
    assert!(session.is_server_running());

    // The session ends (proxy task exits) as it would on a Close/stop_server
    // racing an attach: the attach is enqueued before the proxy task polls
    // its stop trigger. (`remove_session` is synchronous, so on this
    // current-thread runtime the enqueue below still lands while the proxy
    // is alive — exactly the production race.)
    SessionManager::get_instance().remove_session(session.id());

    // FIXED BEHAVIOUR PIN: the attach fails promptly (reply dropped by the
    // proxy's exit drain), instead of stranding forever as pre-fix.
    let res = tokio::time::timeout(Duration::from_secs(3), session.start_server())
        .await
        .expect("start_server must not strand: the queued reply must be dropped on proxy exit");
    assert!(
        res.is_err(),
        "start_server must fail once its proxy is gone (got Ok)"
    );
    // The failed attach must not leave the session claiming a running server.
    assert!(
        !session.is_server_running(),
        "server_running was not rolled back after the failed attach"
    );
    Ok(())
}

/// B3 regression: a legitimate recovery from a *new* source address must
/// re-point the session's `src_ip` at the recovering socket. Pre-fix the
/// session stayed pinned to the address it opened with, so a client that
/// moved networks was still counted against the old IP by
/// `max_sessions_per_remote` and the UDP relay's foreign-source check kept
/// refusing the new address (`src.ip() == session.src_ip().ip()`): the UDP
/// leg died silently while the TCP tunnel recovered fine.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn authenticated_recover_from_new_ip_adopts_the_new_source() -> anyhow::Result<()> {
    let (session, equiv, secret, ticket) = hermetic_session(11).await;
    assert_eq!(session.src_ip(), ORIG_IP);
    push_buffered(&session, 2); // declare a recoverable in-window seq (3, 1)

    // Same flow as `timed_out_recover_preserves_legitimate_recovery` (the leg
    // stays alive so the handshake runs to completion), except the recover
    // socket claims `NEW_IP`.
    let material = derive_tunnel_material(&secret, &ticket)?;
    let (mut client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let handle = tokio::spawn(async move { recover(r, w, &equiv, (3, 1), NEW_IP).await });

    let mut confirm = PacketBuffer::new();
    confirm.set_data(equiv.as_ref())?;
    Crypt::new(&material.key_receive, 0).encrypt(0, equiv.as_ref().len(), &mut confirm)?;
    confirm.write(&mut client).await?;

    tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("the authenticated recover must complete, not hang")
        .expect("recover task panicked")
        .expect("legitimate recovery from a moved client must succeed");

    // FIXED BEHAVIOUR PIN: the session is now attributed to the recovering
    // socket's address, so per-IP caps and the UDP pin follow the client.
    assert_eq!(session.src_ip(), NEW_IP);

    drop(client);
    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}

/// B3 regression (negative): an unauthenticated Recover (equiv id known, no
/// key) that times out must not move the session's `src_ip` to the attacker's
/// address: the address swap is a session-state mutation and, like the window
/// consumption, belongs strictly after the AEAD ticket confirm.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn unauthenticated_recover_timeout_does_not_adopt_the_attacker_ip() {
    let (session, equiv, _secret, _ticket) = hermetic_session(12).await;

    let (client, server) = tokio::io::duplex(1024);
    let (r, w) = tokio::io::split(server);
    let _ = tokio::time::timeout(
        Duration::from_secs(3),
        recover(r, w, &equiv, (3, 1), NEW_IP),
    )
    .await
    .expect("the silent recover must fail on its own timeout, not hang")
    .expect_err("the silent recover must return an error");
    drop(client);

    assert_eq!(
        session.src_ip(),
        ORIG_IP,
        "a timed-out, unauthenticated recover must not re-point the session's source address"
    );
    SessionManager::get_instance().remove_session(session.id());
}

/// B3 regression (negative): a recovery that authenticates but is refused at
/// the window check (a fabricated declaration above the last encrypted frame)
/// must not adopt the peer's address either: only accepted recoveries move
/// session state.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn refused_recover_does_not_adopt_the_peer_ip() {
    let (session, equiv, secret, ticket) = hermetic_session(13).await;
    push_buffered(&session, 2);
    advance_outbound(&session, 2);

    let err = recover_with_valid_confirm(&equiv, &secret, &ticket, (5, 1), NEW_IP)
        .await
        .expect_err("a fabricated declaration must be refused");
    assert!(
        err.to_string().contains("beyond last sent seq"),
        "expected the last-sent refusal, got: {err}"
    );
    assert_eq!(
        session.src_ip(),
        ORIG_IP,
        "a refused recovery must not re-point the session's source address"
    );
    SessionManager::get_instance().remove_session(session.id());
}
