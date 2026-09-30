// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot com
use std::sync::{Arc, atomic::AtomicBool};

use shared::{
    crypt::types::SharedSecret,
    protocol::{Command, ticket::Ticket},
    system::trigger::Trigger,
};

use crate::session::{Session, SessionManager};

use super::*;

const TEST_CHANNEL_ID: u16 = 1; // Currently only supports channel 1

const KEY1: [u8; 32] = [7; 32];
const KEY2: [u8; 32] = [8; 32];

fn make_test_crypts() -> (Crypt, Crypt) {
    // Fixed key for testing
    // Why 2? to ensure each crypt is used where expected
    let key1 = SharedSecret::new(KEY1);
    let key2 = SharedSecret::new(KEY2);

    let inbound = Crypt::new(&key1, 0);
    let outbound = Crypt::new(&key2, 0);

    (inbound, outbound)
}

fn new_session_for_test(remote: &str) -> Session {
    Session::new(
        SharedSecret::new([0u8; 32]),
        Ticket::new_random(),
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec![remote.to_string()],
    )
}

struct FailingStream;

impl tokio::io::AsyncRead for FailingStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("fail")))
    }
}

impl tokio::io::AsyncWrite for FailingStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::other("fail")))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

async fn read_until_close(
    in_crypt: &mut Crypt,
    mut client_stream: &mut (impl tokio::io::AsyncRead + Unpin),
    channel_id: u16,
) -> anyhow::Result<String> {
    let mut received = Vec::new();
    let mut buffer = PacketBuffer::new();
    loop {
        // Read response (also encrypted)
        log::debug!("Waiting for GET response from server");
        let (data, channel) = in_crypt.read(&mut client_stream, &mut buffer).await?;
        if channel == channel_id {
            log::debug!("Received data on channel {}", channel_id);
            received.extend_from_slice(data);
        } else {
            log::debug!("Received data on channel {}: {:?}", channel, data);
            assert_eq!(
                channel, 0,
                "Channel mismatch in response: {} - {:?}",
                channel, data
            );
            let command = Command::from_slice(data)?;
            assert!(matches!(command, Command::CloseChannel { .. }));
            break;
        }
    }
    let response_str = String::from_utf8_lossy(&received);
    Ok(response_str.into_owned())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_basic() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (mut crypt_in, mut _crypt_out) = make_test_crypts(); // Crypt out is for sending TO CLIENT

    // Prepare encrypted message
    let msg = b"16 length text!!";
    let encrypted = {
        let mut msg_packet = PacketBuffer::new();
        msg_packet.set_data(msg).unwrap();
        crypt_in.encrypt(1, msg.len(), &mut msg_packet).unwrap();
        msg_packet
    };

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt_in,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );

    tokio::spawn(async move {
        encrypted.write(&mut client).await.unwrap_or_else(|e| {
            log::error!("Failed to write encrypted data to client: {:?}", e);
        });
        // Client will be closed automatically right here
    });

    inbound.run().await.unwrap();
    let data = rx.recv().unwrap();
    log::debug!("Received data: {:?}:{:?}", data.channel_id, data.payload);

    assert_eq!(data.channel_id, TEST_CHANNEL_ID);
    assert_eq!(data.payload.as_ref(), msg);
    // Stop is set on finish, to ensure other side also stops
    assert!(stop.is_triggered());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_remote_close_before_header() {
    log::setup_logging("debug", log::LogType::Test);

    let session_id = SessionId::new_random();
    let (client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        session_id,
        Arc::new(TrafficCounters::default()),
    );

    drop(client);

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
    assert!(stop.is_triggered());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_read_error() {
    log::setup_logging("debug", log::LogType::Test);

    let (crypt, _) = make_test_crypts();
    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        FailingStream,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );

    let res = inbound.run().await;
    assert!(res.is_err());
    assert!(!stop.is_triggered());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_stop_before_read() {
    log::setup_logging("debug", log::LogType::Test);

    let (_client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );

    stop.trigger();

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_server_stores_recover_packet() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session).unwrap();

    let (_, crypt) = make_test_crypts();
    let stop = Trigger::new();
    let (tx, rx) = flume::bounded(10);

    let mut outbound = TunnelServerOutboundStream::new(
        FailingStream,
        crypt,
        rx,
        stop.clone(),
        *session.id(),
        Arc::new(TrafficCounters::default()),
    );

    // Send a message to the outbound stream, which will cause it to attempt to write and fail

    tx.send_async(PayloadWithChannel {
        channel_id: 0,
        payload: b"test".into(),
    })
    .await
    .unwrap();

    // Must fail with an error
    outbound.run().await.unwrap_err();

    // The session should contain the packet in the recovery buffer
    let ses_rec_buf = session.recovery_buffer();
    let mut buffer = ses_rec_buf.lock();
    assert_eq!(buffer.len(), 1);
    let (item, _old_seq) = buffer.take_unsent_packet().unwrap();
    assert_eq!(item.channel_id, 0);
    assert_eq!(item.payload.as_ref(), b"test");

    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_server_recovers_with_empty_buffer() -> Result<()> {
    // Regression test: when a new outbound stream attaches to a session
    // whose recovery buffer is empty (no failed sends to replay), the
    // drain phase must be a no-op and the stream must continue to
    // process new traffic normally.
    log::setup_logging("debug", log::LogType::Test);
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session).unwrap();

    // Verify the buffer really starts empty.
    assert_eq!(session.recovery_buffer().lock().len(), 0);

    let (_, out_crypt) = make_test_crypts();
    let stop = Trigger::new();
    let (_tx, rx) = flume::bounded(10);
    let (_client, server) = tokio::io::duplex(1024);

    let mut outbound = TunnelServerOutboundStream::new(
        server,
        out_crypt,
        rx,
        stop.clone(),
        *session.id(),
        Arc::new(TrafficCounters::default()),
    );

    let errored = Arc::new(AtomicBool::new(false));
    let outbound_handle = tokio::spawn({
        let stop = stop.clone();
        let errored = errored.clone();
        async move {
            tokio::select! {
                _ = stop.wait_async() => {}
                res = outbound.run() => {
                    if let Err(e) = res {
                        log::error!("Outbound stream failed: {:?}", e);
                        errored.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        }
    });

    // Give the recover_buffer() drain a chance to run and finish.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // The stream must still be alive (not errored) after the empty drain.
    assert!(
        !errored.load(std::sync::atomic::Ordering::Relaxed),
        "outbound stream errored during empty-buffer drain"
    );

    // And the buffer must still be empty (the drain did not invent any
    // items, did not panic, and did not consume a non-existent one).
    assert_eq!(session.recovery_buffer().lock().len(), 0);

    stop.trigger();
    let _ = outbound_handle.await;
    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_server_reads_recover_packet() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session).unwrap();

    let (_, out_crypt) = make_test_crypts();
    let stop = Trigger::new();
    // Ensure tx is alive until the end of the test, so we don't gat any error on task
    let (_tx, rx) = flume::bounded(10);
    let (mut client, server) = tokio::io::duplex(1024);

    // Insert a packet in the recovery buffer, simulating a previous failed send.
    // Scope the MutexGuard so it is dropped before we spawn the outbound
    // task — otherwise the guard crosses an .await and the future loses Send.
    {
        let ses_rec_buf = session.recovery_buffer();
        let mut buffer = ses_rec_buf.lock();
        buffer.push(
            out_crypt.current_seq(),
            PayloadWithChannel {
                channel_id: 0,
                payload: b"test".into(),
            },
        )?;
    }

    let mut outbound = TunnelServerOutboundStream::new(
        server,
        out_crypt,
        rx,
        stop.clone(),
        *session.id(),
        Arc::new(TrafficCounters::default()),
    );

    // Must not fail, so run on ea task to allow check
    let errored = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let stop = stop.clone();
        let errored = errored.clone();
        async move {
            tokio::select! {
                _ = stop.wait_async() => {}
                res = outbound.run() => {
                    if let Err(e) = res {
                        log::error!("Outbound stream failed: {:?}", e);
                        errored.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }

            }
        }
    });

    let mut in_crypt = Crypt::new(&SharedSecret::new(KEY2), 0); // Must use the same key as outbound, as it is the one that encrypts to client

    // Decripted packet should be the same
    let mut buffer = PacketBuffer::new();
    let (data, channel) = in_crypt.read(&mut client, &mut buffer).await?;
    stop.trigger();
    assert_eq!(channel, 0);
    assert_eq!(data, b"test");
    assert!(!errored.load(std::sync::atomic::Ordering::Relaxed));

    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_server_recover_buffer_requeues_on_send_failure() -> Result<()> {
    // Regression test: when a replayed packet fails to send, the failed
    // item AND all remaining drained items must go back into the recovery
    // buffer, in FIFO order, so the next recovery attempt can retry them.
    // Before the fix, drain-then-send dropped everything not yet sent.
    log::setup_logging("debug", log::LogType::Test);
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session).unwrap();

    let (_, out_crypt) = make_test_crypts();
    let stop = Trigger::new();
    let (_tx, rx) = flume::bounded(10);

    // Simulate three previous failed sends queued for replay.
    {
        let rec_buf = session.recovery_buffer();
        let mut buffer = rec_buf.lock();
        for (seq, payload) in [
            (1u64, "one".as_bytes()),
            (2, "two".as_bytes()),
            (3, "three".as_bytes()),
        ] {
            buffer.push(
                seq,
                PayloadWithChannel {
                    channel_id: 0,
                    payload: payload.into(),
                },
            )?;
        }
        assert_eq!(buffer.len(), 3);
    }

    // Writer that fails on the very first write.
    let mut outbound = TunnelServerOutboundStream::new(
        FailingStream,
        out_crypt,
        rx,
        stop.clone(),
        *session.id(),
        Arc::new(TrafficCounters::default()),
    );

    let res = outbound.recover_buffer().await;
    assert!(res.is_err(), "recover_buffer must surface the send error");

    // All three packets (the failed one plus the two not yet attempted)
    // must be back in the buffer, in the original FIFO order.
    let rec_buf = session.recovery_buffer();
    let mut buffer = rec_buf.lock();
    assert_eq!(
        buffer.len(),
        3,
        "recover_buffer lost packets on send failure"
    );
    for (expected_seq, expected_payload) in [(1u64, "one".as_bytes()), (2, b"two"), (3, b"three")] {
        let (item, old_seq) = buffer
            .take_unsent_packet()
            .expect("packet should still be buffered");
        assert_eq!(old_seq, expected_seq);
        assert_eq!(item.payload.as_ref(), expected_payload);
    }

    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_stream_with_invalid_packet() {
    log::setup_logging("debug", log::LogType::Test);

    let (client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let (client_reader, _client_writer) = tokio::io::split(client);
    let (_server_reader, mut server_writer) = tokio::io::split(server);

    let mut inbound = TunnelServerInboundStream::new(
        client_reader,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );

    // Run the inbound stream in the background
    let errored = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let errored = errored.clone();
        async move {
            if inbound.run().await.is_err() {
                errored.store(true, std::sync::atomic::Ordering::SeqCst);
                inbound.server_stop.trigger(); // Ensure stop is triggered on error
            }
        }
    });

    // Prepare invalid packet (too short, and random data)
    // Note: a shorter packet will cause to wait for more data, so we need to make it long enough to trigger the error immediately
    // This is the wrost case, as a larger packet will be parsed as a header, and then fail on payload read, which will trigger the error faster
    let invalid_packet = b"invalidinvalidinvalidinvalid"; // not long enough to be a valid header + payload
    server_writer.write_all(invalid_packet).await.unwrap();

    // Stop shuild be triggered due to error
    assert!(
        stop.wait_timeout_async(std::time::Duration::from_secs(2))
            .await
            .is_ok()
    );
    // Errored should be true
    assert!(errored.load(std::sync::atomic::Ordering::SeqCst));
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_tunnel_inbound() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let ticket = Ticket::new_random();

    // Create the session
    let session = Session::new(
        SharedSecret::new([3u8; 32]),
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec!["echo.free.beeceptor.com:80".to_string()],
    );

    // Add session to manager
    let session = SessionManager::get_instance().add_session(session).unwrap();
    let stop = session.stopper();
    // The test plays the launcher side: private counters seeded at (0, 0),
    // exactly like the real launcher. Sharing the session's live counters
    // would make the test's encrypts collide with the sequence numbers the
    // server's own decrypts advance.
    let material =
        shared::crypt::tunnel::derive_tunnel_material(session.shared_secret(), session.ticket())
            .unwrap();
    let mut out_crypt = Crypt::new(&material.key_receive, 0);
    let mut in_crypt = Crypt::new(&material.key_send, 0);

    let (mut client_side, tunnel_side) = tokio::io::duplex(1024);
    let (tunnel_reader, tunnel_writer) = tokio::io::split(tunnel_side);

    let tunnel = TunnelServerStream::new(*session.id(), tunnel_reader, tunnel_writer);

    // Run the tunnel stream in the background
    tokio::spawn(async move {
        tunnel.run().await.unwrap();
    });
    out_crypt
        .write(
            &mut client_side,
            0, // Control channel
            Command::OpenChannel { channel_id: 1 }.to_bytes().as_slice(),
        )
        .await?;

    out_crypt
        .write(
            &mut client_side,
            TEST_CHANNEL_ID,
            b"GET /echo HTTP/1.0\r\nConnection: Close\r\nHost: echo.free.beeceptor.com\r\n\r\n",
        )
        .await?;

    let data = read_until_close(&mut in_crypt, &mut client_side, 1).await?;
    log::debug!("Received response: {:?}", data);
    assert!(data.contains("HTTP/1.0 200 OK"));

    // Stop the tunnel after some time to avoid hanging the test
    stop.trigger();
    Ok(())
}

// Keep-alive watchdog regressions. Virtual time (start_paused) makes the
// deadlines deterministic: `tokio::time::advance` moves the tokio clock the
// inbound stream's watchdog reads, with no wall-clock sleeping.

/// A half-open leg (peer gone, no FIN/RST) that stops sending frames is torn
/// down after KEEPALIVE_TIMEOUT_SECS, running the normal end-of-stream path
/// (stop triggered so the outbound half ends too).
#[serial_test::serial(manager)]
#[tokio::test(start_paused = true)]
async fn test_server_inbound_keepalive_timeout_ends_stream() {
    log::setup_logging("debug", log::LogType::Test);

    // Client half is kept alive but never writes, so the server read stays
    // pending and the only thing that can move is the watchdog timer.
    let (_client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );

    let handle = tokio::spawn(async move { inbound.run().await });

    // Not expired yet.
    tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS - 1)).await;
    assert!(
        !handle.is_finished(),
        "stream must not die before the deadline"
    );

    // Past the deadline: the watchdog fires and ends the stream cleanly.
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    handle.await.unwrap().unwrap();
    assert!(stop.is_triggered());
}

/// Periodic `Nop` frames keep the leg alive across stretches longer than the
/// deadline, and are consumed on the inbound half (never forwarded to the
/// proxy, which would treat them as an unexpected command and kill the
/// session).
#[serial_test::serial(manager)]
#[tokio::test(start_paused = true)]
async fn test_server_inbound_keepalive_nop_sustains_and_is_not_forwarded() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );
    let handle = tokio::spawn(async move { inbound.run().await });

    // Several keep-alive cycles, each advancing just under the deadline and
    // refreshing it with a `Nop`: total quiet time far exceeds the timeout,
    // yet the stream must stay up.
    for _ in 0..4 {
        tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS - 1)).await;
        client_crypt
            .write(&mut client, 0, Command::Nop.to_bytes().as_slice())
            .await
            .unwrap();
        // Let the inbound drain the buffered frame and refresh its clock.
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(!handle.is_finished(), "Nop must keep the leg alive");
        assert!(
            rx.try_recv().is_err(),
            "Nop must be consumed, not forwarded to the proxy"
        );
    }

    // Stop the keep-alive and let the deadline lapse: now it dies.
    tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS + 1)).await;
    handle.await.unwrap().unwrap();
    assert!(stop.is_triggered());
}

/// Any inbound frame — not just `Nop` — refreshes the deadline, so a launcher
/// that predates the keep-alive (real tunnel traffic only) is not killed while
/// it is actively carrying data, and its payload still reaches the proxy.
#[serial_test::serial(manager)]
#[tokio::test(start_paused = true)]
async fn test_server_inbound_keepalive_data_frame_sustains() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();
    let mut client_crypt = Crypt::new(&SharedSecret::new(KEY1), 0);

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(
        server,
        crypt,
        tx,
        stop.clone(),
        SessionId::new_random(),
        Arc::new(TrafficCounters::default()),
    );
    let handle = tokio::spawn(async move { inbound.run().await });

    for i in 0..3u8 {
        tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS - 1)).await;
        let payload = format!("data-{i}");
        client_crypt
            .write(&mut client, TEST_CHANNEL_ID, payload.as_bytes())
            .await
            .unwrap();
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert!(
            !handle.is_finished(),
            "data traffic must keep the leg alive"
        );
        let got = rx.try_recv().unwrap();
        assert_eq!(got.channel_id, TEST_CHANNEL_ID);
        assert_eq!(got.payload.as_ref(), payload.as_bytes());
    }

    // Quiet past the deadline: the data-driven liveness does not exempt an
    // idle connection from the timeout.
    tokio::time::advance(std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS + 1)).await;
    handle.await.unwrap().unwrap();
    assert!(stop.is_triggered());
}

/// The sequence number stamped into the recovery buffer must be the one the
/// frame actually carries on the wire. Stamping a *prediction*
/// (`current_seq() + 1`) desynchronizes the label from the wire whenever a
/// second holder of the session's shared outbound counter encrypts between
/// the prediction and the encrypt — exactly what a recovery handshake does
/// while an old stream is still live. The buffer then holds labels that no
/// window check can satisfy, and recovery fails.
///
/// Pinned here with a real interleave: the outbound stream sends payloads
/// while another task encrypts under the same shared counter on the same
/// runtime. Drain order is wire order (single writer, FIFO buffer).
#[serial_test::serial(manager)]
#[tokio::test(flavor = "multi_thread")]
async fn recovery_buffer_labels_match_the_sequence_on_the_wire() -> Result<()> {
    let session = new_session_for_test("127.0.0.1:1234");
    let session = SessionManager::get_instance().add_session(session).unwrap();

    let (_, stream_crypt) = session.server_tunnel_crypts()?;
    let stop = Trigger::new();
    let (tx, rx) = flume::bounded(100);
    let (client, server) = tokio::io::duplex(65536);
    let (mut client_reader, _client_write) = tokio::io::split(client);

    let mut outbound = TunnelServerOutboundStream::new(
        server,
        stream_crypt,
        rx,
        stop.clone(),
        *session.id(),
        Arc::new(TrafficCounters::default()),
    );
    let stream_handle = tokio::spawn(async move {
        let _ = outbound.run().await;
    }); // the channel-close exit is expected

    // Independent holder over the session's shared outbound counter: every
    // encrypt consumes a real sequence number that the stream's frames are
    // interleaved with. The first encrypt happens inline, before any payload
    // is queued, so the wire cannot start at seq 1 even on a lucky schedule.
    let mut interloper = {
        let (_in, out) = session.server_tunnel_crypts()?;
        out
    };
    let mut hammer_buf = PacketBuffer::new();
    hammer_buf.set_data(b"interleave").unwrap();
    interloper.encrypt(0, 10, &mut hammer_buf).unwrap();
    let hammer_stop = stop.clone();
    let hammer_handle = tokio::spawn(async move {
        for _ in 0..100u32 {
            if hammer_stop.is_triggered() {
                break;
            }
            interloper.encrypt(0, 10, &mut hammer_buf).unwrap(); // consumes a real seq each round
            tokio::task::yield_now().await;
        }
    });

    let payloads: Vec<Vec<u8>> = (0..20u32)
        .map(|i| format!("rld-{i:03}").repeat(8).into_bytes())
        .collect();
    let expected: Vec<Vec<u8>> = payloads.clone();
    for p in payloads {
        tx.send(PayloadWithChannel {
            channel_id: TEST_CHANNEL_ID,
            payload: p.into(),
        })
        .unwrap();
    }

    // The launcher-side crypt: decrypts server->tunnel frames under key_send.
    let material =
        shared::crypt::tunnel::derive_tunnel_material(session.shared_secret(), session.ticket())?;
    let mut launcher_crypt = Crypt::new(&material.key_send, 0);

    let mut stream_seq: Vec<u64> = Vec::new();
    let mut expect_idx = 0usize;
    while expect_idx < expected.len() {
        let mut header = [0u8; 10];
        client_reader.read_exact(&mut header).await?;
        let length = u16::from_be_bytes([header[8], header[9]]) as usize;
        let mut frame = header.to_vec();
        frame.resize(10 + length, 0);
        client_reader.read_exact(&mut frame[10..]).await?;

        let wire_seq = u64::from_be_bytes(frame[0..8].try_into().unwrap());
        let mut pb = PacketBuffer::new();
        pb.full_buffer_mut()[..frame.len()].copy_from_slice(&frame);
        launcher_crypt.decrypt(&mut pb)?;
        if pb.data() == expected[expect_idx].as_slice() {
            assert_eq!(pb.channel_id(), TEST_CHANNEL_ID);
            stream_seq.push(wire_seq);
            expect_idx += 1;
        }
    }

    drop(tx);
    stop.trigger();
    let _ = stream_handle.await;
    let _ = hammer_handle.await;

    let labels: Vec<u64> = {
        let rec_buf = session.recovery_buffer();
        let mut buf = rec_buf.lock();
        let mut v = Vec::new();
        while let Some((_packet, seq)) = buf.take_unsent_packet() {
            v.push(seq);
        }
        v
    };

    assert!(!stream_seq.is_empty());
    // Every interloper encrypt consumed a real sequence number: the wire seqs
    // of the stream frames are not contiguous.
    assert!(
        stream_seq.windows(2).any(|w| w[1] - w[0] > 1),
        "the interleave did not consume any outbound sequences (test degenerated)"
    );
    assert_eq!(
        labels, stream_seq,
        "recovery-buffer labels drifted from the sequences on the wire"
    );

    SessionManager::get_instance().remove_session(session.id());
    Ok(())
}
