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

use super::*;

use std::net::SocketAddr;

use anyhow::Ok;
use mockito::{Matcher, Server};
use tokio::io::{AsyncWriteExt, DuplexStream};

use shared::{
    consts::TICKET_LENGTH,
    crypt::{
        Crypt,
        tunnel::derive_tunnel_material,
        types::{PacketBuffer, SharedSecret},
    },
    log,
    protocol::{consts::HANDSHAKE_V2_SIGNATURE, handshake::HandshakeCommand},
    system::trigger::Trigger,
    ticket::Ticket,
};

use crate::{config, connection::types::OpenResponse, session::SessionManager};

// Any accesible server for testing would do the job
// as long as it has a known response
const TEST_REMOTE_SERVER: &str = "echo.free.beeceptor.com";
const TEST_REMOTE_PORT: u16 = 80;

// Note: Currently broker only supports one channel, so we use channel 1 that is the one used
// Channel 0 is reserved for control messages
const TEST_STREAM_CHANNEL_ID: u16 = 1;

// Creates a fake mocked broker API for testing
async fn setup_testing_connection(
    proxy_v2: bool,
    multi_channel: bool,
) -> (
    mockito::ServerGuard,
    mockito::Mock,
    DuplexStream,
    Trigger,
    Ticket,
) {
    log::setup_logging("debug", log::LogType::Test);
    log::debug!("Setting up testing connection (proxy_v2={})", proxy_v2);

    let auth_token = "test_token";
    let ticket = Ticket::new_random();
    let fake_src_ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let stop = Trigger::new();

    let mut server = Server::new_async().await;
    let url = server.url() + "/"; // For testing, our base URL will be the mockito server

    // Setup global config for tests
    {
        let config = config::get();
        let mut config = config.write().unwrap();
        config.use_proxy_protocol = Some(proxy_v2);
        config.broker_auth_token = auth_token.to_string();
        config.verify_ssl = Some(false);
        config.ticket_api_url = url.clone();
    }

    let ticket_response_json = if !multi_channel {
        format!(
            r#"
        {{
            "remotes": [
                {{
                    "host": "{}",
                    "port": {},
                    "stream_channel_id": 1
                }}
            ],
            "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }}
        "#,
            TEST_REMOTE_SERVER, TEST_REMOTE_PORT
        )
    } else {
        format!(
            r#"
        {{
            "remotes": [
                {{
                    "host": "{}",
                    "port": {},
                    "stream_channel_id": 1
                }},
                {{
                    "host": "{}",
                    "port": {},
                    "stream_channel_id": 2
                }}
            ],
            "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }}
        "#,
            TEST_REMOTE_SERVER, TEST_REMOTE_PORT, TEST_REMOTE_SERVER, TEST_REMOTE_PORT
        )
    };
    let mock = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ticket_response_json)
        .create();

    // Create a pair of connected TCP streams
    let (client_stream, server_stream) = tokio::io::duplex(1024);

    tokio::spawn(async move {
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        // Simulate server-side handling
        if let Err(e) = handle_connection(server_reader, server_writer, fake_src_ip, proxy_v2).await
        {
            log::error!("Server connection handling failed: {:?}", e);
        }
    });

    SessionManager::get_instance().finish_all_sessions().await;

    // Pass the base url (without /ui) to the API
    (server, mock, client_stream, stop, ticket)
}

fn create_out_int_crypts(ticket: &Ticket) -> anyhow::Result<(Crypt, Crypt)> {
    let (out_crypt, in_crypt) = {
        let shared_secret = SharedSecret::from_hex(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )?;
        let material = derive_tunnel_material(&shared_secret, ticket).unwrap();
        log::debug!(
            "Derived tunnel material: key_receive={:?}, key_send={:?}",
            material.key_receive,
            material.key_send
        );
        (
            Crypt::new(&material.key_receive, 0),
            Crypt::new(&material.key_send, 0),
        )
    };
    Ok((out_crypt, in_crypt))
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_no_proxy_working() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(false, false).await;

    // Send a handshake with Open action
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf.extend_from_slice(ticket.as_ref());
    client_stream.write_all(&signature_buf).await?;
    // Now send the crypted ticket
    let (mut out_crypt, mut in_crypt) = create_out_int_crypts(&ticket)?;

    out_crypt
        .write(&mut client_stream, TEST_STREAM_CHANNEL_ID, ticket.as_ref())
        .await?;
    // Must respond with the session id now
    let mut buffer: PacketBuffer = PacketBuffer::new();
    log::debug!("Waiting for session id response from server");
    let (session_response_data, stream_channel_id) = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;

    let session_response = OpenResponse::from_slice(session_response_data)?;
    assert_eq!(
        session_response.channel_count, 1,
        "Channel mismatch in response"
    );

    // Ensure its on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    let _equiv_session = session_manager
        .get_equiv_session(&session_response.session_id)
        .expect("Session not found");

    // Create a simple GET packet to be encrypted and sent after handshake
    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    let get_request = get_request.as_bytes();
    out_crypt
        .write(&mut client_stream, TEST_STREAM_CHANNEL_ID, get_request)
        .await?;
    // Read response (also encrypted)
    let mut buffer = PacketBuffer::new();
    let (data, stream_channel_id) = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;

    let response_str = String::from_utf8_lossy(data);
    log::info!("Received response: {}", response_str);
    assert!(response_str.contains("HTTP/1.1 200 OK"));
    assert!(
        stream_channel_id == TEST_STREAM_CHANNEL_ID,
        "Channel mismatch in response"
    );

    // Slice some time to tokio tasks to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Should not have any session on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    assert_eq!(session_manager.count(), 0);
    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_no_proxy_handshake_timeout() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(true, false).await;

    // No data sent, will timeout
    tokio::time::sleep(std::time::Duration::from_millis(HANDSHAKE_TIMEOUT_MS + 500)).await;
    // Try to send something after timeout
    let send_result = client_stream.write_all(b"Hello after timeout").await;
    assert!(
        send_result.is_err(),
        "Expected error after handshake timeout"
    );
    // Slice some time to tokio tasks to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Should not have any session on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    assert_eq!(session_manager.count(), 0);
    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_small_handshake_timeout() -> anyhow::Result<()> {
    for len in (0..(HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH)).step_by(10) {
        let (server, mock, mut client_stream, stop, ticket) =
            setup_testing_connection(false, false).await;

        let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH];
        signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
        signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
        signature_buf[HANDSHAKE_V2_SIGNATURE.len() + 1..].copy_from_slice(ticket.as_ref());

        // Send a handshake with Open action, but delay to cause timeout
        let signature_buf = &signature_buf[..len];
        // Does not matter content, we want to timeout
        let send_result = client_stream.write_all(signature_buf).await;
        // Expect no error on write
        assert!(send_result.is_ok(), "Expected no error on write");
        tokio::time::sleep(std::time::Duration::from_millis(HANDSHAKE_TIMEOUT_MS + 50)).await;
        // Try to send something after timeout
        let send_result = client_stream.write_all(b"Hello after timeout").await;
        assert!(
            send_result.is_err(),
            "Expected error after handshake timeout"
        );
    }

    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_ticket_invalid_ticket_crypt() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(false, false).await;

    // Send a handshake with Open action, complete ticket but no further data
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf[HANDSHAKE_V2_SIGNATURE.len() + 1..].copy_from_slice(ticket.as_ref());
    let send_result = client_stream.write_all(&signature_buf).await;
    // Expect no error on write
    assert!(send_result.is_ok(), "Expected no error on write");
    let ticket = Ticket::new_random();
    let (mut out_crypt, _in_crypt) = create_out_int_crypts(&ticket)?;
    let send_result = out_crypt
        .write(&mut client_stream, TEST_STREAM_CHANNEL_ID, ticket.as_ref())
        .await;

    // Expect close on response
    let mut buf = [0u8; 1024];
    let resp = client_stream.read(&mut buf).await;
    log::debug!("Response after invalid ticket crypt: {:?}", resp);
    assert!(
        resp.is_err() || resp.unwrap() == 0,
        "Expected connection close after invalid ticket crypt"
    );

    // Slice some time to tokio tasks to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Should not have any session on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    assert_eq!(session_manager.count(), 0);

    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_proxy_working() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(true, true).await;
    const TEST_STREAM_CHANNEL_ID: u16 = 1;

    // PROXY v2 header:
    // signature (12 bytes)
    // ver_cmd = 0x21 (version 2, command PROXY)
    // fam_proto = 0x11 (INET + STREAM)
    // len = 12 (IPv4 block)
    let proxy_payload = [
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
        0x21, // version=2, command=1
        0x11, // family=1 (IPv4), proto=1 (TCP)
        0x00, 0x0C, // len = 12
        // IPv4 block:
        192, 168, 1, 10, // src IP
        10, 0, 0, 5, // dst IP
        0x1F, 0x90, // src port 8080
        0x00, 0x50, // dst port 80
    ];
    // Send proxy header first
    client_stream.write_all(&proxy_payload).await?;
    // Send a handshake with Open action
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf.extend_from_slice(ticket.as_ref());
    client_stream.write_all(&signature_buf).await?;
    // Now send the crypted ticket
    let (mut out_crypt, mut in_crypt) = {
        let shared_secret = SharedSecret::from_hex(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )?;
        let material = derive_tunnel_material(&shared_secret, &ticket).unwrap();
        log::debug!(
            "Derived tunnel material: key_receive={:?}, key_send={:?}",
            material.key_receive,
            material.key_send
        );
        (
            Crypt::new(&material.key_receive, 0),
            Crypt::new(&material.key_send, 0),
        )
    };
    out_crypt
        .write(&mut client_stream, TEST_STREAM_CHANNEL_ID, ticket.as_ref())
        .await?;
    // Must respond with the session id now
    let mut buffer: PacketBuffer = PacketBuffer::new();
    log::debug!("Waiting for session id response from server");
    let (session_response_data, channel) = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;
    assert_eq!(
        channel, TEST_STREAM_CHANNEL_ID,
        "Channel mismatch in response"
    );
    let session_response = OpenResponse::from_slice(session_response_data)?;
    assert_eq!(
        session_response.channel_count, 2,
        "Channel mismatch in response"
    );
    // Ensure its on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    let _equiv_session = session_manager
        .get_equiv_session(&session_response.session_id)
        .expect("Session not found");

    // Create a simple GET packet to be encrypted and sent after handshake
    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    let get_request = get_request.as_bytes();
    out_crypt.write(&mut client_stream, 1, get_request).await?;
    // Read response (also encrypted)
    log::debug!("Waiting for GET response from server");
    let (data, channel) = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;
    assert_eq!(channel, 1, "Channel mismatch in response");
    let response_str = String::from_utf8_lossy(data);
    log::info!("Received response: {}", response_str);
    assert!(response_str.contains("HTTP/1.1 200 OK"));

    // Send and get from second channel
    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    let get_request = get_request.as_bytes();
    out_crypt.write(&mut client_stream, 2, get_request).await?;
    // Read response (also encrypted)
    log::debug!("Waiting for GET response from server on channel 2");
    let (data, channel) = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;
    assert_eq!(channel, 2, "Channel mismatch in response");
    let response_str = String::from_utf8_lossy(data);
    log::info!("Received response on channel 2: {}", response_str);
    assert!(response_str.contains("HTTP/1.1 200 OK"));

    // Slice some time to tokio tasks to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Should not have any session on session manager
    assert_eq!(session_manager.count(), 0);
    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_invalid_remote() -> anyhow::Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let auth_token = "test_token";
    let ticket = Ticket::new_random();
    let fake_src_ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let stop = Trigger::new();
    let proxy_v2 = false;

    let mut server = Server::new_async().await;
    let url = server.url() + "/"; // For testing, our base URL will be the mockito server

    // Setup global config for tests
    {
        let config = config::get();
        let mut config = config.write().unwrap();
        config.use_proxy_protocol = Some(proxy_v2);
        config.broker_auth_token = auth_token.to_string();
        config.verify_ssl = Some(false);
        config.ticket_api_url = url.clone();
    }

    let ticket_response_json = format!(
        r#"
        {{
            "host": "{}",
            "port": {},
            "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }}
        "#,
        TEST_REMOTE_SERVER, TEST_REMOTE_PORT
    );
    let mock = server
        .mock(
            "GET",
            Matcher::Regex(format!("/{}/{}/{}", ticket.as_str(), r".+", auth_token)),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ticket_response_json)
        .create();

    // Create a pair of connected TCP streams
    let (client_stream, server_stream) = tokio::io::duplex(1024);

    // Invoking handle_connection directly with invalid remote address
    // will fail. Ensure not hanged test with a timeout
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        // Simulate server-side handling
        handle_connection(server_reader, server_writer, fake_src_ip, false).await
    })
    .await?;

    assert!(
        result.is_err(),
        "Expected connection failure due to invalid remote"
    );

    Ok(())
}
