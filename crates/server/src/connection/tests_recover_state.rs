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

/// Characterisation: with a shared `seq_in`, `Crypt::decrypt` is check-then-act
/// — it loads `current_seq`, runs the whole AES-GCM verification, and only then
/// `fetch_max`es the counter. Two inbound crypts that share one counter
/// therefore BOTH accept the same frame if they overlap (recover handshake
/// crypt vs the live stream crypt, or a replaced stream vs its replacement).
/// Consequence is a duplicated frame delivered to the proxy, not a crypto
/// break: both holders already possess the per-session key.
#[test]
fn shared_inbound_counter_can_duplicate_accept_a_frame() {
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
