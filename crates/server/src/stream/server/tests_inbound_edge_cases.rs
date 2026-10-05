// Regression tests for the launcher keepalive watchdog and the inbound
// stream's command interception.
use std::sync::Arc;

use shared::{crypt::types::SharedSecret, protocol::Command, system::trigger::Trigger};

use crate::session::{SessionId, TrafficCounters};

use super::*;

const KEY1: [u8; 32] = [7; 32];

fn inbound_pair() -> (
    TunnelServerInboundStream<tokio::io::DuplexStream>,
    tokio::io::DuplexStream,
    Trigger,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let crypt = Crypt::new(&SharedSecret::new(KEY1), 0);
    let (tx, _rx) = flume::bounded(64);
    let stop = Trigger::new();
    let stream = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );
    (stream, client, stop)
}

// ---------------------------------------------------------------------------
// `Nop` is intercepted ONLY on channel 0. On a data channel it is ordinary
// payload and must be forwarded (no silent drop).
// ---------------------------------------------------------------------------
#[serial_test::serial(manager)]
#[tokio::test]
async fn nop_on_data_channel_is_forwarded_as_data() {
    let (mut inbound, mut client, stop) = inbound_pair();
    let (tx, rx) = flume::bounded(16);
    inbound.sender = tx;
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);

    let handle = tokio::spawn(async move { inbound.run().await });
    client_crypt
        .write(&mut client, 1, Command::Nop.to_bytes().as_slice())
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv_async())
        .await
        .expect("data-channel Nop must be forwarded, not consumed")
        .unwrap();
    assert_eq!(got.channel_id, 1);
    assert_eq!(got.payload.as_ref(), Command::Nop.to_bytes().as_slice());
    stop.trigger();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// A channel-0 command that is neither `Close` nor `Nop` falls through and
// is forwarded to the proxy. The proxy treats it as an unexpected command
// and tears the session down; capture the stream half of that behaviour.
// ---------------------------------------------------------------------------
#[serial_test::serial(manager)]
#[tokio::test]
async fn unexpected_channel0_command_is_forwarded_to_proxy() {
    let (mut inbound, mut client, stop) = inbound_pair();
    let (tx, rx) = flume::bounded(16);
    inbound.sender = tx;
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);

    let handle = tokio::spawn(async move { inbound.run().await });
    // `Ok` (0x00) is a valid command byte but unexpected from the launcher.
    client_crypt
        .write(&mut client, 0, Command::Ok.to_bytes().as_slice())
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv_async())
        .await
        .expect("unexpected channel-0 command reaches the proxy")
        .unwrap();
    assert_eq!(got.channel_id, 0);
    assert_eq!(got.payload.as_ref(), Command::Ok.to_bytes().as_slice());
    stop.trigger();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// A malformed channel-0 payload (command byte that does not parse) is also
// forwarded to the proxy rather than being dropped.
// ---------------------------------------------------------------------------
#[serial_test::serial(manager)]
#[tokio::test]
async fn malformed_channel0_payload_is_forwarded_to_proxy() {
    let (mut inbound, mut client, stop) = inbound_pair();
    let (tx, rx) = flume::bounded(16);
    inbound.sender = tx;
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);

    let handle = tokio::spawn(async move { inbound.run().await });
    client_crypt.write(&mut client, 0, &[0xEEu8]).await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv_async())
        .await
        .expect("malformed channel-0 payload reaches the proxy")
        .unwrap();
    assert_eq!(got.channel_id, 0);
    assert_eq!(got.payload.as_ref(), &[0xEEu8]);
    stop.trigger();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// A `Nop` shorter than the deadline sustains the LEG indefinitely: 100
// cycles of (deadline - 1s) is ~900 s of quiet with no teardown. This is
// the per-stream keep-alive watchdog only; the bare `inbound_pair` stream
// here owns no registered session, so the session-level data-idle cap
// (`session_idle_data_timeout_secs`, see `session::manager`) does not
// apply — `Nop`s alone sustain the leg but never the session.
// ---------------------------------------------------------------------------
#[serial_test::serial(manager)]
#[tokio::test(start_paused = true)]
async fn nops_sustain_the_leg_far_beyond_the_keepalive_deadline() {
    let (mut inbound, mut client, stop) = inbound_pair();
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);
    let handle = tokio::spawn(async move { inbound.run().await });

    for _ in 0..100 {
        tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS - 1)).await;
        client_crypt
            .write(&mut client, 0, Command::Nop.to_bytes().as_slice())
            .await
            .unwrap();
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(
            !handle.is_finished(),
            "a Nop per deadline must sustain the leg indefinitely"
        );
    }
    stop.trigger();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// A client that holds the key can put ANY `seq` on the wire (the tag is
// computed over that seq, so it verifies). `seq = u64::MAX` used to drive the
// inbound counter update `seq + 1` past u64::MAX: a panic under debug
// overflow-checks that faulted the whole inbound stream, or a wrapped/wedged
// counter in release. The fix rejects the frame before the counter advance, so
// the inbound stream survives and stays usable.
// ---------------------------------------------------------------------------
#[serial_test::serial(manager)]
#[tokio::test]
async fn wire_max_seq_frame_is_rejected_without_faulting_the_inbound_stream() {
    let (mut inbound, mut client, stop) = inbound_pair();
    let (tx, rx) = flume::bounded(16);
    inbound.sender = tx;
    // Client crypt seeded one below the maximum: its next encrypt is seq=MAX.
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), u64::MAX - 1);

    let handle = tokio::spawn(async move { inbound.run().await });
    client_crypt.write(&mut client, 1, b"boom").await.unwrap();

    // The offending frame is a rejected decrypt: `inbound.run()` returns Err
    // (the read propagates it) but must NOT panic, and nothing is forwarded.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), handle)
        .await
        .expect("inbound stream must return on the rejected frame, not hang");
    let join_result = outcome.expect("inbound stream task panicked on seq=u64::MAX");
    assert!(
        join_result.is_err(),
        "the rejected seq=u64::MAX frame must surface as a stream error, not Ok"
    );
    assert!(
        rx.is_empty(),
        "a rejected frame must never be forwarded to the proxy"
    );
    drop(client);
    stop.trigger();
}

// ---------------------------------------------------------------------------
// The rejected seq=u64::MAX frame must not move the inbound counter. A fresh
// receiver at a normal counter rejects the poison frame and still decrypts a
// later legitimate frame, proving the counter was not advanced/wedged by the
// rejected one. This mirrors the server-side `Crypt` the inbound stream shares
// with its session.
// ---------------------------------------------------------------------------
#[test]
fn inbound_counter_untouched_by_rejected_max_seq_frame() {
    use shared::crypt::types::PacketBuffer;

    // Receiver at the normal start counter (what the stream hands the wire).
    let mut receiver = Crypt::new(&SharedSecret::new(KEY1), 0);

    // Poison: a correctly-tagged seq=u64::MAX frame.
    let mut attacker = Crypt::new(&SharedSecret::new(KEY1), u64::MAX - 1);
    let mut poison = PacketBuffer::from(b"boom".as_slice());
    attacker.encrypt(1, 4, &mut poison).unwrap();
    assert!(receiver.decrypt(&mut poison).is_err());
    assert_eq!(
        receiver.current_seq(),
        0,
        "a rejected seq=u64::MAX must not move the shared inbound counter"
    );

    // A later legitimate frame (seq 1) still decrypts cleanly on the receiver.
    let mut legit = Crypt::new(&SharedSecret::new(KEY1), 0);
    let mut lb = PacketBuffer::from(b"hi".as_slice());
    legit.encrypt(1, 2, &mut lb).unwrap();
    receiver
        .decrypt(&mut lb)
        .expect("legit frame after a rejected seq=MAX must still decrypt");
    assert_eq!(receiver.current_seq(), 2);
}
