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

// Authors: Adolfo Gómez, dkmaster at dkmon dot compub mod broker;
use std::sync::Arc;

use super::*;

use shared::{
    consts::TICKET_LENGTH, crypt::types::SharedSecret, system::trigger::Trigger, ticket::Ticket,
};

use crate::session::{ServerEndpoints, Session, SessionId, SessionManager};

async fn create_test_server_stream() -> (Arc<Session>, tokio::io::DuplexStream) {
    log::setup_logging("debug", log::LogType::Test);

    let ticket = Ticket::new([0x40u8; TICKET_LENGTH]);

    // Create the session
    let session = Session::new(
        SharedSecret::new([3u8; 32]),
        ticket,
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
    );

    // Add session to manager
    let session = SessionManager::get_instance().add_session(session).unwrap();

    let (client_side, tunnel_side) = tokio::io::duplex(1024);
    let (tunnel_reader, tunnel_writer) = tokio::io::split(tunnel_side);

    let tss = TunnelClientStream::new(*session.id(), 1, tunnel_reader, tunnel_writer);

    tokio::spawn(async move {
        tss.run().await.unwrap();
    });

    (session, client_side)
}

async fn get_server_stream_components(
    session_id: &SessionId,
) -> Result<(Trigger, ServerEndpoints)> {
    let (stop, channels) =
        if let Some(session) = SessionManager::get_instance().get_session(session_id) {
            (
                session.stop_trigger(),
                session.server_sender_receiver().await?,
            )
        } else {
            log::warn!("Session {:?} not found, aborting stream", session_id);
            anyhow::bail!("Session not found");
        };
    Ok((stop, channels))
}

async fn init_server_test() -> (SessionId, tokio::io::DuplexStream, Trigger, ServerEndpoints) {
    log::setup_logging("debug", log::LogType::Test);

    let (session, client) = create_test_server_stream().await;

    let (stop, endpoints) = get_server_stream_components(session.id()).await.unwrap();

    (*session.id(), client, stop, endpoints)
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_read_and_send() {
    let (mut client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(1, server, tx, stop.clone());

    tokio::spawn(async move {
        client.write_all(b"hello").await.unwrap();
    });

    inbound.run().await.unwrap();

    assert_eq!(rx.recv().unwrap().1, b"hello");
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_receive_and_write() {
    log::setup_logging("debug", log::LogType::Test);

    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    tokio::spawn({
        let stop = stop.clone();

        async move {
            log::debug!("Sending data through outbound stream");
            tx.send_async(b"hello".to_vec()).await.unwrap();
            // Wait for stop signal
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        log::debug!("Running outbound stream");
        outbound.run().await.unwrap();
    });

    // lee lo que escribió el outbound
    let mut buf = [0u8; 5];
    log::debug!("Waiting to read from server side");
    server.read_exact(&mut buf).await.unwrap();
    stop.trigger();
    log::debug!("Read data: {:?}", &buf);
    assert_eq!(&buf, b"hello");
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_inbound_remote_close() {
    let (client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(1, server, tx, stop.clone());

    // Close the client side to simulate remote close
    drop(client);

    inbound.run().await.unwrap();

    // Should not receive any data
    let result = rx.try_recv();
    assert!(
        result.is_err(),
        "Expected no data, but received some: {:?}",
        result
    );
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_inbound_read_error() {
    struct FailingReader;

    impl tokio::io::AsyncRead for FailingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::other("fail")))
        }
    }

    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(1, FailingReader, tx, stop.clone());

    let res = inbound.run().await;
    assert!(res.is_err());
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_channel_closed() {
    let (client, _server) = tokio::io::duplex(1024);
    let (_tx, rx) = flume::bounded::<Vec<u8>>(10);
    let stop = Trigger::new();

    drop(_tx); // cerrar canal

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    let res = outbound.run().await;
    assert!(res.is_err());
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_stop_before_data() {
    let (client, _server) = tokio::io::duplex(1024);
    let (_tx, rx) = flume::bounded::<Vec<u8>>(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    stop.trigger(); // detener antes de arrancar

    outbound.run().await.unwrap();
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_backpressure() {
    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded::<Vec<u8>>(1);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    // Enviar dos mensajes: el segundo se bloqueará hasta que el primero se consuma
    tokio::spawn({
        let stop = stop.clone();
        async move {
            tx.send_async(b"one".to_vec()).await.unwrap();
            tx.send_async(b"two".to_vec()).await.unwrap();
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    let mut buf = [0u8; 3];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"one");

    let mut buf2 = [0u8; 3];
    server.read_exact(&mut buf2).await.unwrap();
    assert_eq!(&buf2, b"two");

    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_full_tunnel_echo() {
    let (client_side, mut server) = tokio::io::duplex(1024);
    let (server_side, mut client) = tokio::io::duplex(1024);

    let (tx_in, rx_in) = flume::bounded::<(u16, Vec<u8>)>(10);
    let (tx_out, rx_out) = flume::bounded::<Vec<u8>>(10);

    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(1, server_side, tx_in, stop.clone());
    let mut outbound = TunnelClientOutboundStream::new(client_side, rx_out, stop.clone());

    // Task inbound
    tokio::spawn(async move {
        inbound.run().await.unwrap();
    });

    // Task outbound
    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    // Simulate proxy that echoes back
    tokio::spawn({
        let stop = stop.clone();
        async move {
            while let Ok(msg) = rx_in.recv_async().await {
                tx_out.send_async(msg.1).await.unwrap();
            }
            stop.trigger();
        }
    });

    // Write on client side
    client.write_all(b"ping").await.unwrap();

    // Read b
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).await.unwrap();

    assert_eq!(&buf, b"ping");
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_inbound_multiple_packets() {
    log::setup_logging("debug", log::LogType::Test);

    let (mut client, server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelClientInboundStream::new(1, server, tx, stop.clone());

    tokio::spawn(async move {
        inbound.run().await.unwrap();
    });

    tokio::spawn(async move {
        // Without delay, the three may come in a single read, or 2-1, etc.
        client.write_all(b"111").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // small delay to simulate separate packets
        client.write_all(b"222").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // small delay to simulate separate packets
        client.write_all(b"333").await.unwrap();
    });

    assert_eq!(rx.recv_async().await.unwrap().1, b"111");
    assert_eq!(rx.recv_async().await.unwrap().1, b"222");
    assert_eq!(rx.recv_async().await.unwrap().1, b"333");
    log::debug!("All packets received");
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_outbound_multiple_packets() {
    let (client, mut server) = tokio::io::duplex(1024);
    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut outbound = TunnelClientOutboundStream::new(client, rx, stop.clone());

    tokio::spawn({
        let stop = stop.clone();
        async move {
            tx.send_async(b"one".to_vec()).await.unwrap();
            tx.send_async(b"two".to_vec()).await.unwrap();
            tx.send_async(b"three".to_vec()).await.unwrap();
            stop.wait_async().await;
        }
    });

    tokio::spawn(async move {
        outbound.run().await.unwrap();
    });

    let mut buf = [0u8; 5];

    server.read_exact(&mut buf[..3]).await.unwrap();
    assert_eq!(&buf[..3], b"one");

    server.read_exact(&mut buf[..3]).await.unwrap();
    assert_eq!(&buf[..3], b"two");

    server.read_exact(&mut buf[..5]).await.unwrap();
    assert_eq!(&buf[..5], b"three");
    stop.trigger();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_client_stream_valid_packets() -> Result<()> {
    let (session_id, mut client, stop, endpoints) = init_server_test().await;

    let sent_msg = b"Hello, server!";
    client.write_all(sent_msg).await.unwrap();

    // Should receive on tx on time
    log::debug!("Waiting to receive message on server side");

    // Should receive on tx on time
    let (channel_id, recv_msg) =
        tokio::time::timeout(std::time::Duration::from_secs(2), endpoints.rx.recv_async())
            .await??;
    assert_eq!(recv_msg, sent_msg);

    log::debug!(
        "Session Manager State 1: {:?}",
        *SessionManager::get_instance()
    );

    // Stop should not be triggered
    assert!(!stop.is_triggered());

    log::debug!("Sending response back to client");

    // Send response back to client
    let response_msg = b"Hello, client!";
    endpoints
        .tx
        .send_async((channel_id, response_msg.to_vec()))
        .await
        .unwrap();

    log::debug!("Waiting to receive response on client side");
    // Read from client the forwarded mesage
    let mut buf = vec![0u8; response_msg.len()];
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, response_msg);

    // Trigger stop to end the test
    log::debug!(
        "Session Manager State 2: {:?}",
        *SessionManager::get_instance()
    );
    log::debug!("Triggering stop");

    stop.trigger();
    // Wait a bit and theres session should be closed

    let _result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if SessionManager::get_instance()
                .get_session(&session_id)
                .is_none()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    log::debug!(
        "Session Manager State 3: {:?}",
        *SessionManager::get_instance()
    );

    let session = SessionManager::get_instance().get_session(&session_id);
    assert!(session.is_none(), "Session should be removed after stop",);

    Ok(())
}
