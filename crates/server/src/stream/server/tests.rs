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
use std::sync::Arc;

use shared::{
    consts,
    crypt::{build_header, consts::HEADER_LENGTH, types::SharedSecret},
    protocol::{Command, ticket::Ticket},
    system::trigger::Trigger,
};

use crate::session::{Session, SessionManager};

use super::*;

const TEST_CHANNEL_ID: u16 = 1; // Currently only supports channel 1

fn make_test_crypts() -> (Crypt, Crypt) {
    // Fixed key for testing
    // Why 2? to ensure each crypt is used where expected
    let key1 = SharedSecret::new([7; 32]);
    let key2 = SharedSecret::new([8; 32]);

    let inbound = Crypt::new(&key1, 0);
    let outbound = Crypt::new(&key2, 0);

    (inbound, outbound)
}

async fn read_until_close(
    in_crypt: &mut Crypt,
    mut client_stream: &mut (impl tokio::io::AsyncRead + Unpin),
    channel_id: u16,
    stop: &Trigger,
) -> anyhow::Result<String> {
    let mut received = Vec::new();
    let mut buffer = PacketBuffer::new();
    loop {
        // Read response (also encrypted)
        log::debug!("Waiting for GET response from server");
        let (data, channel) = in_crypt.read(stop, &mut client_stream, &mut buffer).await?;
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
        let mut msg_packet = PacketBuffer::from_slice(msg);
        let encrypted = crypt_in.encrypt(1, msg.len(), &mut msg_packet).unwrap();
        encrypted.to_vec()
    };
    let counter = crypt_in.current_seq();
    let length = encrypted.len() as u16;

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(server, crypt_in, tx, stop.clone());

    let mut header = [0u8; HEADER_LENGTH];
    build_header(counter, length, &mut header).unwrap();

    tokio::spawn(async move {
        client.write_all(&header).await.unwrap();
        client.write_all(&encrypted).await.unwrap();
        // Client will be closed automatically right here
    });

    inbound.run().await.unwrap();
    let data = rx.recv().unwrap();
    log::debug!("Received data: {:?}:{:?}", data.channel_id, data.payload);

    assert_eq!(data.channel_id, TEST_CHANNEL_ID);
    assert_eq!(data.payload.as_ref(), msg);
    // Stop is set on TunnelServerStream, so here must be unset
    assert!(!stop.is_triggered());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_remote_close_before_header() {
    log::setup_logging("debug", log::LogType::Test);

    let (client, server) = tokio::io::duplex(1024);
    let (crypt, _) = make_test_crypts();

    let (tx, rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

    drop(client);

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
    assert!(!stop.is_triggered());
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_server_inbound_read_error() {
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

    log::setup_logging("debug", log::LogType::Test);

    let (crypt, _) = make_test_crypts();
    let (tx, _rx) = flume::bounded(10);
    let stop = Trigger::new();

    let mut inbound = TunnelServerInboundStream::new(FailingReader, crypt, tx, stop.clone());

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

    let mut inbound = TunnelServerInboundStream::new(server, crypt, tx, stop.clone());

    stop.trigger();

    inbound.run().await.unwrap();

    assert!(rx.try_recv().is_err());
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

    let mut inbound = TunnelServerInboundStream::new(client_reader, crypt, tx, stop.clone());

    // Run the inbound stream in the background
    let errored = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let errored = errored.clone();
        async move {
            if inbound.run().await.is_err() {
                errored.store(true, std::sync::atomic::Ordering::SeqCst);
                inbound.stop.trigger(); // Ensure stop is triggered on error
            }
        }
    });

    // Prepare invalid packet (too short, and random data)
    // Note: a shorter packet will cause to wait for more data, so we need to make it long enough to trigger the error immediately
    // This is the wrost case, as a larger packet will be parsed as a header, and then fail on payload read, which will trigger the error faster
    let invalid_packet = b"invalid"; // not long enough to be a valid header + payload
    server_writer.write_all(invalid_packet).await.unwrap();

    // Stop shuild be triggered due to error
    assert!(
        stop.wait_timeout_async(std::time::Duration::from_secs(
            consts::CRYPT_PACKET_TIMEOUT_SECS + 1
        ))
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
    let stop = session.stop_trigger();
    let (mut out_crypt, mut in_crypt) = session.server_tunnel_crypts().unwrap();

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

    let data = read_until_close(&mut in_crypt, &mut client_side, 1, &stop).await?;
    log::debug!("Received response: {:?}", data);
    assert!(data.contains("HTTP/1.0 200 OK"));

    // Stop the tunnel after some time to avoid hanging the test
    stop.trigger();
    Ok(())
}
