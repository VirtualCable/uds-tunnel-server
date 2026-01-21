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

use mockito::Server;
use tokio::io::AsyncWriteExt;

use crate::{
    config,
    connection::handshake::HandshakeCommand,
    consts::HANDSHAKE_V2_SIGNATURE,
    crypt::{
        Crypt,
        tunnel::derive_tunnel_material,
        types::{PacketBuffer, SharedSecret},
    },
    log,
    system::trigger::Trigger,
    ticket::Ticket,
};

// Any accesible server for testing would do the job
// as long as it has a known response
const TEST_REMOTE_SERVER: &str = "echo.free.beeceptor.com";
const TEST_REMOTE_PORT: u16 = 80;

// Creates a fake mocked broker API for testing
async fn setup_testing_connection(
    auth_token: &str,
    ticket: &Ticket,
    ip: &SocketAddr,
) -> (mockito::ServerGuard, mockito::Mock) {
    log::setup_logging("debug", log::LogType::Test);

    let mut server = Server::new_async().await;
    let url = server.url() + "/"; // For testing, our base URL will be the mockito server

    // Setup global config for tests
    {
        let config = config::get();
        let mut config = config.write().unwrap();
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
            format!(
                "/{}/{}/{}",
                ticket.as_str(),
                ip.ip().to_string().as_str(),
                auth_token
            )
            .as_str(),
        )
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ticket_response_json)
        .create();

    // Pass the base url (without /ui) to the API
    (server, mock)
}

#[tokio::test]
async fn test_connection_no_proxy_working() -> anyhow::Result<()> {
    log::setup_logging("debug", log::LogType::Test);
    let auth_token = "test_token";
    let ticket = Ticket::new_random();
    let fake_src_ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (server, mock) = setup_testing_connection(auth_token, &ticket, &fake_src_ip).await;
    let stop = Trigger::new();

    // Create a pair of connected TCP streams
    let (mut client_stream, server_stream) = tokio::io::duplex(1024);

    tokio::spawn(async move {
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        // Simulate server-side handling
        if let Err(e) = handle_connection(server_reader, server_writer, fake_src_ip, false).await {
            log::error!("Server connection handling failed: {:?}", e);
        }
    });
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
    out_crypt.write(&mut client_stream, ticket.as_ref()).await?;

    // Create a simple GET packet to be encrypted and sent after handshake
    let get_request =
        format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", TEST_REMOTE_SERVER);
    let get_request = get_request.as_bytes();
    out_crypt.write(&mut client_stream, get_request).await?;
    // Read response (also encrypted)
    let mut buffer = PacketBuffer::new();
    let data = in_crypt
        .read(&stop, &mut client_stream, &mut buffer)
        .await?;

    let response_str = String::from_utf8_lossy(&data);
    log::info!("Received response: {}", response_str);
    assert!(response_str.contains("HTTP/1.1 200 OK"));

    // Slice some time to tokio tasks to complete
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // Should not have any session on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    assert_eq!(session_manager.count(), 0);
    Ok(())
}
