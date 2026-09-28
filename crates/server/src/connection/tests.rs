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

use super::*;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use mockito::{Matcher, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use shared::{
    crypt::{
        Crypt,
        tunnel::derive_tunnel_material,
        types::{PacketBuffer, SharedSecret},
    },
    log,
    protocol::{
        Command, PayloadWithChannel, consts::HANDSHAKE_V2_SIGNATURE, consts::TICKET_LENGTH,
        consts::TUNNEL_AUTH_HEADER, handshake::HandshakeCommand, ticket::Ticket,
    },
    system::trigger::Trigger,
};

use crate::{config, connection::types::OpenResponse, session::SessionManager};

// Any accesible server for testing would do the job
// as long as it has a known response
const TEST_REMOTE_SERVER: &str = "echo.free.beeceptor.com";
const TEST_REMOTE_PORT: u16 = 80;

const TEST_REMOTE_SERVER2: &str = "echo.free.beeceptor.com";
const TEST_REMOTE_PORT2: u16 = 80;

// Note: Currently broker only supports one channel, so we use channel 1 that is the one used
// Channel 0 is reserved for control messages
const TEST_STREAM_CHANNEL_ID: u16 = 1;

// Ticket used to encrypt sample responses
pub const TICKET_ID: &str = "c6s9FAa5fhb854BVMckqUBJ4hOXg2iE5i1FYPCuktks4eNZD";

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
        config.dangerous_disable_ssl_verify = Some(false);
        config.ticket_api_url = url.clone();
        // The optional caps are exercised by dedicated tests that set them
        // explicitly; never leak them from a previously-serialized test.
        config.max_sessions = None;
        config.max_sessions_per_remote = None;
    }

    let ticket_response_json = if !multi_channel {
        // Decripted values are:
        // {
        //     "remotes": [
        //         {
        //             "host": "echo.free.beeceptor.com",
        //             "port": 80,
        //             "stream_channel_id": 1
        //         }
        //     ],
        //     "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
        //     "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        // }
        r#"{
            "algorithm": "AES-256-GCM",
            "ciphertext": "kZSEYJN/z3zZTkmEZBsCwIVhG5MmtxTRZijJ/PGsX27TGv3qv7086X9PWXN3HFjyiZ22LbZD2fDJP7GlxtwjWQkqh8vx0c/E16xMFHDQuEItEJVZFUD3qcFHHlVuhukV7N1IDGer6YC6cxvE+J1z9ei6+97hEkp6S9g9SJmj+YbaCR63qdiKFpXDcs863ZFnFdUy3lWvpC42hD16xwsYcIOePXVknFlqGQ05eKK8NFH13T5l5890UUn5hcefGO6fBwQxU5Z09BYXZ3TFxpNCoFOoCE36b8SCZIRqggI0nN5zBsSeyv+BnQIVlerA5Pmt68pSfutGreYttS30n7ViYaLuBSKUBeS7NhZ/giyZF9A56StDOa2HH5/31Jja8cyFTKLi4XIcWPFCt7cMu4ADD3hifh3OSXDzs9QUmJmXWGasrIArYnhHfQxBPH7KQ9TihjywthRG5orJX9guJlYFdgHDtlYSyY0PTzHZzrTZXBCTBi/jo8T1EpHB+vGua/C1nssbeDPUqzNnCgkIhTXr7AmOaXMy3xZ8cfCsL+mMzbMlQUfhkK22S/7G4gi5t0/24iN/qN1xK2qo/rpPPwZ1W/+3NFgQ1saO/yiy8BIgfd1NDj6+d4fRrghETerH4gIrh64onmAu3YSGyDG+Tmo7DMIWjzwDEYxb30AGWe+0+FCDvU/l8JnNDTqdlZqimJmBj0zZ3UR4TORm+bGt4ODNL8DYIqLIvTYXsDGiWzwsLm+v2mGgij4OerDWuvaNn3suGoKsaY1nJwHtVRXXy6ViNIOwnlh18rUdtsZ9B2pVvQs/uQTR+b8YJOMKTGEFEO3d5XluLgFuwZ7QRtEGKCy7flkGVEzWygjeULtFydHviS8khcOkpWRRjpe3ID6h9leUIG1wdYO7lVvOVf82X611Ex5/g/RNl6ZHySup3oFIcTwdka5ypbN77nQg90Ti/+DLb3sLeEYy6vZW8BBOV9gRYWiHDrzxTeEOS7irZ2f+bzB6P3ff7lROEVHdAojPuZjXN3i9SXNdcMqcRUq4AGiIIomB8Dg0P4zH7ns9qEhgAZb117s18ugi8dJBIRcb0SeOPaHCjDyezS2gv6RVksxIhpfdscEtTLlyjjxwMJxMJq0C42FZFYIm1AwYXJ1NAAX4bg8wMuA3Q8qUIOpgoq9i9LQitzeGIBZj336MizvgVLJUEKrWsXv77p2fYUh6Hc/8J5JMjp5ifM7X2SmEBl0coBpaHMLrTw9pdPEWJbJWn4k6tpbDFlmJtaTviF2bToJ398vwlsyUT/eO9tKIZuMM7GoxYlbHtH+8ttTaajbpfufuIreMW3WdJjjnPBJJ79kodHbMR+UPxwuIEuUIqWFNGQN4/6TnbSRNUsMpso84IbiIPFbQ8goAlZds9cf65cRvpkHitLDTFqh6tEmOUwdM2vlZqcjMk17dvaRgZDAbfPw=",
            "data": "9F8HAK6YkJ2kIcX+IhsPhvV3NISJC05z1W84zsK+apovP7tD7tpIkK5RLYNCKGDuJgNzQqMU1Wdj/B+YTLOcpJaBMzyU6K93Ah3GdtTKe4LD+9U5j3Li5RJ6GAJ2EmWfl1eDQBM+by7HwNnYln3mbMr46D25EfB3bV1I0T4VVnZTRQU0fkzllI8oSFcbrJH6XZ7/3kOBhrf7vGz5XWExpbVGbZXfwb4/OqLFrekVsqP7Zkld+UxWJI8r593PoielOu7OzcjuIi9qy45scBl/AHIrczf4X7Uj3aRUFLLBIam7JivSDlLeuFkO9NMpQ3Rr8o5vViTY7pTw"
        }"#
    } else {
        // Decripted values are:
        // {
        //     "remotes": [
        //         {
        //             "host": "echo.free.beeceptor.com",
        //             "port": 80,
        //             "stream_channel_id": 1
        //         },
        //         {
        //             "host": "echo.free.beeceptor.com",
        //             "port": 80,
        //             "stream_channel_id": 2
        //         }
        //     ],
        //     "notify": "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
        //     "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        // }
        r#"{
            "algorithm": "AES-256-GCM",
            "ciphertext": "FHeg74x1Mt4pFTakGOdORqKb6KllCd7XoP7Pqq0CFC9+RjmqdNjZn0SKfw2FWKQxAb7+EScYBejkn4FekFXoB8QQmkzRdxa3UWcCb6HzI2ZWH2lBg9sIeaQRzUthSwbKEgEcFHM+xXv3bEMfToaeNyfYPeI/XnlSSwZ83Hh/okY0J5BM0jSDjeggIn6aAtX5zOvGHYX9gAu+6ppOfCxxm+gDsxswmwAXCIcWB2OjjWTyJMcRhJ2xrORAyq+ThKH5cBp3yPFRI77ogNgZbCmQnc/X72lFxCfPgNY0grR2fwTSPB3lu/LwLW3JrMesG4vY64R77+od+8CvPRA7GWpryhAS0l+F4ddjWJLEJgS9LK1mufcvUBwPaVe61Ojq70hwngPbq0T2zIrwhIfAuV3QTfWh3XTP+7l78BOo06N7Z2RF4aYu0z3jyz5uZ1vCL1ANqGOGzQNXP5XU8662buI0IbOzYZCB2PemiqfJ5JIZDovibl8jLyi8rBDpkmMaPLdvVItfBwWfW+txMrFVCXBbBdSUiBqjCty9kDHDqxKCeoO6S7b0YbLhlQlh+JfTvw3uS+wpVJbSXavoDEOFk+46DT5Za7Ne/hzMTuhTIYY8OslVxgCPfoFMxNWbJb9IEAmZgGuDH4JmXbmZKkNmZDMfNxT7zMf/0jCJGOIMIHujwdMDybcI4DwXDq2UOdbKh6RZzzFs9RkGVkpyYkCMeXScfxLjRk5bZ0eZEovTHTLsQhDPN+mJpN/ocInoLN6rZAnNw/AZd1V7TiVcIxKrvxgTlGcPutMy582yQdQW/Mdg2scLPKmuZCRuXdsKkqe/ib+K/G+yVEmq59Spn8mAxFaxxcSlgomLYYw8KEHnLqIAWZgzGVUYw3GCoe/vFHsaIhnpymZY1S93kKxqJv8JX24Dv5u8cl/8O49r9xYCUkZNxmOIdCcg86dS/8FfTGYXuPISvlH7keGtbcpAyR9jsBPVS0ZPr1sIIiuFjEyOkoClc/5/6FJi4gJvT3Gspywy4V/94TJPtdZ3dwFt7A/H6Cfege3ZN/HlNX8raBFI9dUoAENJmIdrO0p0VnpUD5YAnx7BX0ZUPL+9FhOHdxpw8fg8RQnyhht7KTHgbV7NBv1smftF2W8gKC6S/28cguGb2ksY46cDH1BRSBk8tFATKdivrLCwEe+ehJ3+xW503Hg9Fy/FStybUF6LIMKqkWj1qMZu5ax30IcsG9c9dn98QDEZ3rsTd+OKH8P+JtwUhuyISnnFnuxqFg9Sz/xi2L19cbUxO0fWOnDAqYIxQfLFoD74X+W5lDqGJ0zmNqTjjulKZSevkseZCRX7R3b8tPELdxca9CG6Lwr0YtEwXDh8uL5s0UFTKv++mGhTD3a+KWTujwDpi49mkBo+CoEXw0uIoamwyugsfO497q/Gp/5RcUP1Ue7A4XQ9SMTG+tyh4cQufAw=",
            "data": "RFXKo7mYtbrcgK/OhbYsVAKcKF37Zg6vaIDnzkYq1oCVwBEFP2l+4Mp5Cu0L8lVJqjAyvAWQkv4S9zEs/n9hVRZC0kgEmrWP66yP5niaZShXUiZ5s9A+bRqvjltrSIiYNCRp1mb34+K1145oxu3fJ9eePc5Cqov1pXW4qoGZgP6Hvgr3e6AHRkuhb/NDy0fCBoAAqfaWQ8WOrx0zNcd76VMF45fXrsXJkZivaV4rLzaDMW/KL9ft88qbeoJc3us/xrMkt/vZqEyoWmMNc51aDZsRJ55CvCiJbVUKniIs6yU+JXSGWSmeZ7d+aW5IDvjeHeAoI08z+8hvo32bzPfFEmab19ffmAANIjkKWF83ZusvsXI7A2rlji6eI2nvZVcPPcNaGjelfvc+8r1qczEYoa1Y+YbQ0buyhIWNyFHDkf+wlQ=="
        }"#
    };
    let mock = server
        .mock("POST", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(ticket_response_json)
        .create();

    // Create a pair of connected TCP streams
    let (client_stream, server_stream) = tokio::io::duplex(1024);
    SessionManager::get_instance().finish_all_sessions().await;

    tokio::spawn(async move {
        let (server_reader, server_writer) = tokio::io::split(server_stream);
        // Simulate server-side handling
        if let Err(e) = handle_connection(server_reader, server_writer, fake_src_ip).await {
            log::error!("Server connection handling failed: {:?}", e);
        }
    });

    // Pass the base url (without /ui) to the API
    (
        server,
        mock,
        client_stream,
        stop,
        Ticket::new(TICKET_ID.as_bytes().try_into().unwrap()),
    )
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

async fn wait_for_session_manager_empty() -> Result<()> {
    let session_manager = SessionManager::get_instance();
    for _ in 0..10 {
        if session_manager.count() == 0 {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("Session manager not empty after waiting");
}

/// Poll until the stop matcher has captured at least one request body
/// (detached notifications get a chance to run). Returns false after ~5s.
async fn wait_for_stop_capture(captured: &StopCapture) -> bool {
    for _ in 0..50 {
        if captured.lock().unwrap().is_some() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

/// Drive the client side of the Open handshake up to and including the
/// encrypted ticket echo. The server answers with an `OpenResponse` on the
/// happy path; on early rejections (caps) it just closes the stream, so
/// callers that expect a response must read it themselves.
async fn send_open_handshake(
    client_stream: &mut DuplexStream,
    ticket: &Ticket,
    out_crypt: &mut Crypt,
) -> anyhow::Result<()> {
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf.extend_from_slice(ticket.as_ref());
    client_stream.write_all(&signature_buf).await?;
    out_crypt
        .write(client_stream, TEST_STREAM_CHANNEL_ID, ticket.as_ref())
        .await?;
    Ok(())
}

/// Broker stop matcher for the fixed test notify ticket ("B" * 48).
///
/// The `start` POST of the same flow never matches this body (`command`
/// differs), so the mock can be registered up front without stealing the
/// handshake's start response. When `captured` is given, the stop request
/// body is recorded as JSON so the test can pin the reported stats after
/// the fact (mockito exposes no public per-mock hit body query).
type StopCapture = Arc<Mutex<Option<serde_json::Value>>>;

fn broker_stop_mock(
    server: &mut mockito::ServerGuard,
    captured: Option<StopCapture>,
) -> mockito::Mock {
    let mock = server
        .mock("POST", "/")
        .match_header(
            TUNNEL_AUTH_HEADER,
            mockito::Matcher::Regex("Bearer sk-".into()),
        )
        .match_body(Matcher::PartialJson(serde_json::json!({
            "command": "stop",
            "ticket": "B".repeat(TICKET_LENGTH),
        })));
    let mock = if let Some(captured) = captured {
        mock.with_body_from_request(move |request| {
            if let Ok(body) = request.utf8_lossy_body()
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&body)
            {
                *captured.lock().unwrap() = Some(json);
            }
            b"{}".to_vec()
        })
    } else {
        mock
    };
    // One and exactly one stop POST per closed session.
    mock.expect(1).with_status(200).create()
}

/// Same matcher, for the negative contract: asserts that no second stop
/// ever reaches the broker (e.g. after the shutdown path already claimed
/// the notification and the `Session` Arc is dropped later).
fn broker_no_extra_stop_mock(server: &mut mockito::ServerGuard) -> mockito::Mock {
    server
        .mock("POST", "/")
        .match_header(
            TUNNEL_AUTH_HEADER,
            mockito::Matcher::Regex("Bearer sk-".into()),
        )
        .match_body(Matcher::PartialJson(serde_json::json!({
            "command": "stop",
            "ticket": "B".repeat(TICKET_LENGTH),
        })))
        .expect(0)
        .with_status(200)
        .create()
}

fn captured_json(captured: &StopCapture) -> serde_json::Value {
    captured
        .lock()
        .unwrap()
        .clone()
        .expect("stop body never captured")
}

/// Snapshots the session-cap knobs of the global config and restores them
/// on drop, so cap-exercising tests cannot leak `max_sessions = Some(0)`
/// into the serialized manager tests that run next with this config.
struct SessionCapRestore(Option<usize>, Option<usize>);

impl SessionCapRestore {
    fn snapshot() -> Self {
        let config = config::get();
        let config = config.read().unwrap();
        Self(config.max_sessions, config.max_sessions_per_remote)
    }
}

impl Drop for SessionCapRestore {
    fn drop(&mut self) {
        let config = config::get();
        let mut config = config.write().unwrap();
        config.max_sessions = self.0;
        config.max_sessions_per_remote = self.1;
    }
}

async fn read_until_close(
    in_crypt: &mut Crypt,
    mut client_stream: &mut DuplexStream,
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
            assert!(matches!(command, Command::CloseChannel { channel_id, .. }));
            break;
        }
    }
    let response_str = String::from_utf8_lossy(&received);
    Ok(response_str.into_owned())
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
    let (session_response_data, stream_channel_id) =
        in_crypt.read(&mut client_stream, &mut buffer).await?;

    log::debug!(
        "Received session response on channel {}: {:?}",
        stream_channel_id,
        session_response_data
    );
    let session_response = OpenResponse::from_slice(session_response_data)?;
    assert_eq!(
        session_response.channel_count, 1,
        "Channel mismatch in response"
    );

    log::debug!(
        "Session established with id {:?}",
        session_response.session_id
    );
    // Ensure its on session manager
    let session_manager = crate::session::SessionManager::get_instance();
    let _equiv_session = session_manager
        .get_equiv_session(&session_response.session_id)
        .expect("Session not found");

    // Now, open the remote channel (1)
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            Command::OpenChannel { channel_id: 1 }.to_bytes().as_slice(),
        )
        .await?;

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
    let response =
        read_until_close(&mut in_crypt, &mut client_stream, TEST_STREAM_CHANNEL_ID).await?;

    log::info!("Received response: {}", response);
    assert!(response.contains("HTTP/1.1 200 OK"));

    let session_manager = crate::session::SessionManager::get_instance();
    // The session should still be there, as we have not closed server side
    assert_eq!(session_manager.count(), 1);
    // Close the server side

    // Send Close message. Without this, the session would wait to a possible recover
    let close_msg = Command::Close.to_message();
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            close_msg.payload.as_ref(),
        )
        .await?;
    // tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    client_stream.shutdown().await?;
    wait_for_session_manager_empty().await?;
    Ok(())
}

/// End-to-end guard for the broker session-stop report: after a full
/// tunnel with traffic, closing the session must make the server POST a
/// `command: stop` with the notify ticket the broker handed out at start.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_notifies_broker_stop_on_close() -> anyhow::Result<()> {
    let (mut server, _mock, mut client_stream, _stop, ticket) =
        setup_testing_connection(false, false).await;

    // Registered before the session can close: the cap-reject path
    // notifies the broker the moment `start` returns and the connect
    // guard drops, so waiting to install the matcher would race it.
    let captured: StopCapture = Arc::new(Mutex::new(None));
    let stop_mock = broker_stop_mock(&mut server, Some(captured.clone()));

    let (mut out_crypt, mut in_crypt) = create_out_int_crypts(&ticket)?;
    send_open_handshake(&mut client_stream, &ticket, &mut out_crypt).await?;
    let mut buffer: PacketBuffer = PacketBuffer::new();
    let (session_response_data, _channel) = in_crypt.read(&mut client_stream, &mut buffer).await?;
    let session_response = OpenResponse::from_slice(session_response_data)?;

    let session_manager = crate::session::SessionManager::get_instance();
    let session = session_manager
        .get_equiv_session(&session_response.session_id)
        .expect("Session not found");

    // Open the remote channel (1) and relay some traffic so the stop
    // report carries a real snapshot instead of zeros.
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            Command::OpenChannel { channel_id: 1 }.to_bytes().as_slice(),
        )
        .await?;

    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    out_crypt
        .write(
            &mut client_stream,
            TEST_STREAM_CHANNEL_ID,
            get_request.as_bytes(),
        )
        .await?;
    let response =
        read_until_close(&mut in_crypt, &mut client_stream, TEST_STREAM_CHANNEL_ID).await?;
    assert!(response.contains("HTTP/1.1 200 OK"));

    let (sent, recv) = session.traffic().snapshot();
    assert!(sent > 0, "upload not counted");
    assert!(recv > 0, "download not counted");

    // Close the session definitively. The stop fires from
    // `Session::Drop`, i.e. only when the *last* Arc goes away — this
    // test holds one extra reference, so release it first.
    drop(session);
    let close_msg = Command::Close.to_message();
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            close_msg.payload.as_ref(),
        )
        .await?;
    client_stream.shutdown().await?;
    wait_for_session_manager_empty().await?;

    // The stop report is spawned detached from the session teardown; poll
    // briefly for the mock to be hit. `.expect(1)` + `assert()` then pin
    // the exactly-once contract: no duplicate stop, none missing.
    assert!(
        wait_for_stop_capture(&captured).await,
        "broker never received the stop notification for the closed session"
    );
    let body = captured_json(&captured);
    assert_eq!(body["sent"], serde_json::json!(sent), "reported sent");
    assert_eq!(body["recv"], serde_json::json!(recv), "reported recv");
    stop_mock.assert();
    Ok(())
}

/// Contract test: when the per-IP cap rejects a connect, the broker
/// reservation made by the successful `start` is still released — the RAII
/// guard in `connect` reports a zero-traffic stop, so the ticket row does
/// not linger until its TTL.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_per_ip_cap_rejection_notifies_stop() -> anyhow::Result<()> {
    let (mut server, _mock, mut client_stream, _stop, ticket) =
        setup_testing_connection(false, false).await;

    // Every source IP already at cap: the first connect is rejected. The
    // guard restores the global knobs on exit (even through a panic) so
    // the serialized manager tests never inherit a zero cap.
    let _cap_restore = SessionCapRestore::snapshot();
    config::get().write().unwrap().max_sessions_per_remote = Some(0);

    // The stop may arrive while the handshake is still being refused;
    // the matcher must be live before the guard can drop.
    let captured: StopCapture = Arc::new(Mutex::new(None));
    let stop_mock = broker_stop_mock(&mut server, Some(captured.clone()));

    let (mut out_crypt, _in_crypt) = create_out_int_crypts(&ticket)?;
    send_open_handshake(&mut client_stream, &ticket, &mut out_crypt).await?;
    // The server rejects before creating a Session; the stream just dies.
    let _ = client_stream.shutdown().await;

    assert!(
        wait_for_stop_capture(&captured).await,
        "broker reservation from a cap-rejected start was never stopped"
    );
    let body = captured_json(&captured);
    assert_eq!(body["sent"], serde_json::json!(0), "no traffic relayed");
    assert_eq!(body["recv"], serde_json::json!(0), "no traffic relayed");
    stop_mock.assert();
    Ok(())
}

/// Contract test: when the global session cap refuses the registration,
/// the Session — which already owns the notify ticket handed over by the
/// connect guard — is dropped and its `Drop` reports the zero-traffic stop.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_add_session_cap_rejection_notifies_stop() -> anyhow::Result<()> {
    let (mut server, _mock, mut client_stream, _stop, ticket) =
        setup_testing_connection(false, false).await;

    // add_session always refuses: registration happens after the
    // guard has handed the notify ticket to the Session. Restored on
    // exit via the RAII guard (see the per-IP cap test above).
    let _cap_restore = SessionCapRestore::snapshot();
    config::get().write().unwrap().max_sessions = Some(0);

    let captured: StopCapture = Arc::new(Mutex::new(None));
    let stop_mock = broker_stop_mock(&mut server, Some(captured.clone()));

    let (mut out_crypt, _in_crypt) = create_out_int_crypts(&ticket)?;
    send_open_handshake(&mut client_stream, &ticket, &mut out_crypt).await?;
    let _ = client_stream.shutdown().await;

    assert!(
        wait_for_stop_capture(&captured).await,
        "a session dropped by the global cap never told the broker"
    );
    let body = captured_json(&captured);
    assert_eq!(body["sent"], serde_json::json!(0), "no traffic relayed");
    assert_eq!(body["recv"], serde_json::json!(0), "no traffic relayed");
    stop_mock.assert();
    Ok(())
}

/// Contract test: server shutdown (`finish_all_sessions`) must deliver the
/// stop reports with the final traffic snapshot, awaited (not detached),
/// so the process can exit with the broker fully informed. Also pins that
/// the shutdown claim suppresses the later `Session::Drop` notification.
#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_finish_all_sessions_reports_stop_with_stats() -> anyhow::Result<()> {
    let (mut server, _mock, mut client_stream, _stop, ticket) =
        setup_testing_connection(false, false).await;

    let captured: StopCapture = Arc::new(Mutex::new(None));
    let stop_mock = broker_stop_mock(&mut server, Some(captured.clone()));

    let (mut out_crypt, mut in_crypt) = create_out_int_crypts(&ticket)?;
    send_open_handshake(&mut client_stream, &ticket, &mut out_crypt).await?;
    let mut buffer: PacketBuffer = PacketBuffer::new();
    let (session_response_data, _channel) = in_crypt.read(&mut client_stream, &mut buffer).await?;
    let session_response = OpenResponse::from_slice(session_response_data)?;

    let session_manager = crate::session::SessionManager::get_instance();
    let session = session_manager
        .get_equiv_session(&session_response.session_id)
        .expect("Session not found");

    // Open the channel, send traffic, and wait until the server-side
    // counters observe it: the stop snapshot must contain the bytes that
    // were actually relayed, and the pump task adds them before any
    // later claim of the notification can run.
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            Command::OpenChannel { channel_id: 1 }.to_bytes().as_slice(),
        )
        .await?;

    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    out_crypt
        .write(
            &mut client_stream,
            TEST_STREAM_CHANNEL_ID,
            get_request.as_bytes(),
        )
        .await?;
    let response =
        read_until_close(&mut in_crypt, &mut client_stream, TEST_STREAM_CHANNEL_ID).await?;
    assert!(response.contains("HTTP/1.1 200 OK"));

    // No `Close` command: the session stays registered — the shutdown case.
    let (sent, recv) = session.traffic().snapshot();
    assert!(sent > 0, "upload not counted");
    assert!(recv > 0, "download not counted");

    // `finish_all_sessions` awaits every report, so once it returns the
    // mock must already have its single hit — no polling needed.
    session_manager.finish_all_sessions().await;
    assert!(
        stop_mock.matched(),
        "shutdown did not deliver the broker stop report synchronously"
    );
    let body = captured_json(&captured);
    assert_eq!(body["sent"], serde_json::json!(sent), "reported sent");
    assert_eq!(body["recv"], serde_json::json!(recv), "reported recv");

    // This test still holds the last Arc: dropping it now must NOT send a
    // second stop (the shutdown path already claimed the notification).
    // `expect(0)` asserts it: a stray second POST to this matcher exceeds
    // the bound and the assert() below panics.
    drop(session);
    let after_drop = broker_no_extra_stop_mock(&mut server);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    after_drop.assert();
    stop_mock.assert();
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
async fn test_connection_ticket_confirm_timeout_no_leak() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(false, false).await;

    // Complete the plain handshake, but never send the crypted ticket
    // confirmation: the connect handshake times out after one second.
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf[HANDSHAKE_V2_SIGNATURE.len() + 1..].copy_from_slice(ticket.as_ref());
    client_stream.write_all(&signature_buf).await?;

    // Wait past the ticket-confirm timeout so the error path runs
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    // The session registered before the timeout must not leak
    let session_manager = crate::session::SessionManager::get_instance();
    assert_eq!(session_manager.count(), 0);

    Ok(())
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn test_connection_handshake_seq_matches_client_state() -> anyhow::Result<()> {
    let (server, mock, mut client_stream, stop, ticket) =
        setup_testing_connection(false, false).await;

    // Send a handshake with Open action
    let mut signature_buf = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH];
    signature_buf[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    signature_buf[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    signature_buf[HANDSHAKE_V2_SIGNATURE.len() + 1..].copy_from_slice(ticket.as_ref());
    client_stream.write_all(&signature_buf).await?;

    // Drive the handshake exactly like the real tunnel client does
    // (client v5 proxy::connect): crypts seeded at (0, 0), the encrypted
    // ticket echo sent on channel 0 (its seq=1 frame captured for the
    // replay check below), then the OpenResponse read back (the server
    // reflects the echo channel). The client REUSES these same crypts for
    // the rest of the connection, so their post-handshake current_seq is
    // the wire contract the server must sync to.
    let shared_secret =
        SharedSecret::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")?;
    let material = derive_tunnel_material(&shared_secret, &ticket)?;
    let mut client_outbound = Crypt::new(&material.key_receive, 0);
    let mut client_inbound = Crypt::new(&material.key_send, 0);

    let mut echo = PacketBuffer::new();
    echo.set_data(ticket.as_ref())?;
    client_outbound.encrypt(0, ticket.as_ref().len(), &mut echo)?;
    let echo_frame = echo.clone(); // the exact bytes that went on the wire
    echo.write(&mut client_stream).await?;

    let mut buffer: PacketBuffer = PacketBuffer::new();
    let (resp_data, resp_channel) = client_inbound.read(&mut client_stream, &mut buffer).await?;
    let response = OpenResponse::from_slice(resp_data)?;
    assert_eq!(resp_channel, 0);

    // Let `connect` spawn the live stream (its crypts share the session
    // counters; the handshake already advanced them in place).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let session_manager = SessionManager::get_instance();
    let session = session_manager
        .get_equiv_session(&response.session_id)
        .expect("Session not found");

    // Contract assertion: the server-side session seqs must equal the live
    // client crypt state after the handshake (inbound: decrypt left
    // last_used + 1; outbound: encrypt pre-incremented, so last_used).
    // Derived from the client side on purpose: this pins the wire contract
    // instead of hardcoding a value that merely happens to match one impl.
    let client_state = (client_inbound.current_seq(), client_outbound.current_seq());
    assert_eq!(session.seqs(), client_state);

    // Compatibility: the next legitimate frame from the client's live
    // crypt (its seq=2, e.g. an OpenChannel on the control channel) must
    // decrypt fine with a fresh server-side crypt built from the session
    // seqs — the sync must never seed above what the client will send.
    let payload = Command::OpenChannel { channel_id: 1 }.to_bytes();
    let mut next = PacketBuffer::new();
    next.set_data(payload.as_slice())?;
    client_outbound.encrypt(0, payload.len(), &mut next)?;
    let (mut server_inbound, _) = session.server_tunnel_crypts()?;
    let mut next_buf = next.clone();
    server_inbound.decrypt(&mut next_buf)?;
    assert_eq!(next_buf.channel_id(), 0);
    assert_eq!(next_buf.data(), payload.as_slice());

    // Anti-replay: a captured copy of the seq=1 handshake echo must NOT be
    // admitted as a post-handshake packet. The shared inbound counter is at
    // 2 after the handshake (decrypt set last-used + 1), so a replay of the
    // handshake echo is rejected by the floor check, not merely by parsing.
    let mut replay = echo_frame;
    let (mut replay_crypt, _) = session.server_tunnel_crypts()?;
    assert!(
        replay_crypt.decrypt(&mut replay).is_err(),
        "replayed handshake echo (seq 1) must be rejected after the handshake"
    );

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
    let (session_response_data, channel) = in_crypt.read(&mut client_stream, &mut buffer).await?;
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

    // Now, open the remote channel (1)
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            Command::OpenChannel { channel_id: 1 }.to_bytes().as_slice(),
        )
        .await?;

    // Create a simple GET packet to be encrypted and sent after handshake
    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    let get_request = get_request.as_bytes();
    out_crypt.write(&mut client_stream, 1, get_request).await?;
    // Read response (also encrypted)
    log::debug!("Waiting for GET response from server on channel 1");
    let response = read_until_close(&mut in_crypt, &mut client_stream, 1).await?;
    log::info!("Received response: {}", response);
    assert!(response.contains("HTTP/1.1 200 OK"));

    // And open channel 2
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            Command::OpenChannel { channel_id: 2 }.to_bytes().as_slice(),
        )
        .await?;

    // Send and get from second channel
    let get_request = format!(
        "GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        TEST_REMOTE_SERVER
    );
    let get_request = get_request.as_bytes();
    out_crypt.write(&mut client_stream, 2, get_request).await?;
    // Read response (also encrypted)
    log::debug!("Waiting for GET response from server on channel 2");
    let response = read_until_close(&mut in_crypt, &mut client_stream, 2).await?;
    log::info!("Received response on channel 2: {}", response);
    assert!(response.contains("HTTP/1.1 200 OK"));

    let session_manager = crate::session::SessionManager::get_instance();
    // The session should still be there, as we have not closed server side
    assert_eq!(session_manager.count(), 1);
    // Close the server side
    // Send Close message. Without this, the session would wait to a possible recover
    let close_msg = Command::Close.to_message();
    out_crypt
        .write(
            &mut client_stream,
            0, // Control channel
            close_msg.payload.as_ref(),
        )
        .await?;

    client_stream.shutdown().await?;

    wait_for_session_manager_empty().await?;
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
        config.dangerous_disable_ssl_verify = Some(false);
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
        handle_connection(server_reader, server_writer, fake_src_ip).await
    })
    .await?;

    assert!(
        result.is_err(),
        "Expected connection failure due to invalid remote"
    );

    Ok(())
}

// A malformed Recover handshake must not empty the recovery buffer and
// tear down the recovered session. The Recover path validates
// `in_seqs.0` against the buffer's window before mutating any session
// state, so the checks below run before any network side-effects.

#[serial_test::serial(manager)]
#[tokio::test]
async fn recover_validation_runs_before_network() -> Result<()> {
    // A bogus ticket + in_seq=0 must fail before any read on the stream.
    // We deliberately do not write anything to the client half of the
    // duplex: if validation does not reject, recover will block reading
    // and the test will time out.
    use crate::connection::recover::recover;

    let (client, server_io) = tokio::io::duplex(64);
    let (r, w) = tokio::io::split(server_io);
    let fake_ticket = Ticket::new([0xBBu8; TICKET_LENGTH]);
    let ip: SocketAddr = "127.0.0.1:1234".parse().unwrap();

    let res = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recover(r, w, &fake_ticket, (0, 0), ip),
    )
    .await
    .expect("recover should reject in_seq=0 without blocking on the stream")
    .expect_err("recover must return an error for in_seq=0");

    drop(client);
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// Regression test: Recover must not hand the launcher leg a nonce that a
// still-live server stream has already used.
//
// Drives the REAL code path (an Open that leaves a live `TunnelServerStream`
// running, then a Recover over a second connection) and captures the exact
// on-the-wire bytes of each leg. The server kills the live stream instantly
// and every crypt it builds shares the session's live per-direction counters,
// so the recovered crypts continue strictly past every nonce the replaced
// stream actually emitted — without any drain/publish step. Two streams
// encrypting different plaintexts under the same (key, seq) pair would be an
// AES-GCM keystream reuse; the shared atomics make that impossible by
// construction. The kill must also leave the replacement stream's proxy
// attachment intact: exactly one server stream is launcher-facing at any
// time, and proxy data flows over the new one after recovery.
// ────────────────────────────────────────────────────────────────────────

/// Read one raw AEAD frame off the wire: 8-byte seq + 2-byte length header,
/// followed by `length` bytes (ciphertext || 16-byte tag).
async fn read_raw_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 10];
    r.read_exact(&mut header).await?;
    let length = u16::from_be_bytes([header[8], header[9]]) as usize;
    let mut frame = header.to_vec();
    frame.resize(10 + length, 0);
    r.read_exact(&mut frame[10..]).await?;
    Ok(frame)
}

fn frame_seq(f: &[u8]) -> u64 {
    u64::from_be_bytes(f[0..8].try_into().unwrap())
}

/// Decrypt a captured raw frame with the provided crypt; returns
/// (channel_id, payload).
fn decrypt_raw(crypt: &mut Crypt, f: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut pb = PacketBuffer::new();
    pb.full_buffer_mut()[..f.len()].copy_from_slice(f);
    crypt.decrypt(&mut pb)?;
    Ok((pb.channel_id(), pb.data().to_vec()))
}

#[serial_test::serial(config, manager)]
#[tokio::test]
async fn recover_kills_live_stream_and_continues_shared_counters() -> anyhow::Result<()> {
    // ---- connection A: the real Open handshake, live stream left running ---
    let (server, _mock, mut client_a, _stop_a, ticket) =
        setup_testing_connection(false, false).await;

    // Public handshake: signature + Open + ticket.
    let mut sig = vec![0u8; HANDSHAKE_V2_SIGNATURE.len() + 1 + TICKET_LENGTH];
    sig[..HANDSHAKE_V2_SIGNATURE.len()].copy_from_slice(HANDSHAKE_V2_SIGNATURE);
    sig[HANDSHAKE_V2_SIGNATURE.len()] = HandshakeCommand::Open.into();
    sig[HANDSHAKE_V2_SIGNATURE.len() + 1..].copy_from_slice(ticket.as_ref());
    client_a.write_all(&sig).await?;

    let shared_secret =
        SharedSecret::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")?;
    let material = derive_tunnel_material(&shared_secret, &ticket)?;

    // Client crypts seeded at (0,0), exactly like the real launcher.
    let mut client_a_out = Crypt::new(&material.key_receive, 0);
    let mut client_a_in = Crypt::new(&material.key_send, 0);

    // Ticket echo on channel 0 (the real client does this over the AEAD leg).
    let mut echo = PacketBuffer::new();
    echo.set_data(ticket.as_ref())?;
    client_a_out.encrypt(0, ticket.as_ref().len(), &mut echo)?;
    echo.write(&mut client_a).await?;

    // Capture + decrypt the OpenResponse (raw frame = on-the-wire bytes).
    let open_frame = read_raw_frame(&mut client_a).await?;
    assert_eq!(frame_seq(&open_frame), 1);
    let (_, open_pt) = decrypt_raw(&mut client_a_in, &open_frame)?;
    let response = OpenResponse::from_slice(&open_pt)?;

    // Let `connect` spawn the live stream (the handshake crypts already
    // advanced the shared session counters).
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let session = SessionManager::get_instance()
        .get_equiv_session(&response.session_id)
        .expect("session not found");
    // Production post-connect state: (inbound, outbound) == (2, 1).
    assert_eq!(session.seqs(), (2, 1));

    // Drive the LIVE TunnelServerStream to emit two outbound frames
    // (seq 2 and seq 3) by feeding the proxy->launcher channel it reads.
    let (launcher_tx, _) = session.get_proxy_channels();
    let p_live1 = vec![0xA1u8; 90];
    let p_live2 = vec![0xB2u8; 90];
    launcher_tx
        .send_async(PayloadWithChannel::new(1, &p_live1))
        .await?;
    launcher_tx
        .send_async(PayloadWithChannel::new(1, &p_live2))
        .await?;

    // What an on-path observer captures on connection A.
    let live1 = read_raw_frame(&mut client_a).await?;
    let live2 = read_raw_frame(&mut client_a).await?;
    assert_eq!(frame_seq(&live1), 2, "live stream first frame");
    assert_eq!(frame_seq(&live2), 3, "live stream second frame");

    // ---- connection B: the REAL recover path via `handle_connection` ------
    let (mut client_b, server_b) = tokio::io::duplex(8192);
    let ip: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        let (r, w) = tokio::io::split(server_b);
        let _ = handle_connection(r, w, ip).await;
    });

    // Recover handshake: signature + Recover + equiv id + in_seq + out_seq.
    // in_seq=3 => requested = 2, inside the live stream's recovery window
    // [2, 3] (frames at seq 2 and 3 are buffered for retransmission).
    let mut rec = Vec::new();
    rec.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
    rec.push(HandshakeCommand::Recover.into());
    rec.extend_from_slice(response.session_id.as_ref());
    rec.extend_from_slice(&3u64.to_be_bytes());
    rec.extend_from_slice(&1u64.to_be_bytes());
    client_b.write_all(&rec).await?;

    // Echo the recover session id over the recovered AEAD leg. The server's
    // inbound counter is at 2 after connect (the ticket echo on A was
    // decrypted at seq 1), so the next inbound frame it accepts is seq 2 —
    // the client's next encrypt must land on seq 2.
    let mut client_b_out = Crypt::new(&material.key_receive, 1);
    let mut echo2 = PacketBuffer::new();
    echo2.set_data(response.session_id.as_ref())?;
    client_b_out.encrypt(0, response.session_id.as_ref().len(), &mut echo2)?;
    echo2.write(&mut client_b).await?;

    // What an on-path observer captures on connection B (the OpenResponse).
    // The live stream's last outbound seq is 3, and the recover handshake's
    // shared outbound crypt writes the OpenResponse at seq 4 — strictly past
    // every frame the killed stream emitted (2 and 3): no (key, seq)
    // collision between the replaced and the recovering leg.
    let rec_frame = read_raw_frame(&mut client_b).await?;
    assert_eq!(
        frame_seq(&rec_frame),
        4,
        "the OpenResponse must continue past the live stream's last seq (3), not reuse it"
    );
    let mut client_b_in = Crypt::new(&material.key_send, 3);
    let (_, rec_pt) = decrypt_raw(&mut client_b_in, &rec_frame)?;
    let rec_response = OpenResponse::from_slice(&rec_pt)?;
    // Wire contract (unchanged for the launcher): the response carries the
    // pre-confirm snapshot, and the session's shared counters already moved
    // past it (the confirm decrypt and the OpenResponse encrypt advanced
    // them). The pair is asserted deterministically once the retransmission
    // and the fresh frame are captured below — mid-handshake it races with
    // the new stream's first outbound seq.
    assert_eq!(rec_response.inbound_seq, 2);
    assert_eq!(rec_response.outbound_seq, 3);
    assert!(frame_seq(&rec_frame) > frame_seq(&live1));
    assert!(frame_seq(&rec_frame) > frame_seq(&live2));

    // ---- post-recover liveness on connection B: the new stream owns the
    // proxy data path, and the killed one cannot tear it down. ----
    // The recover request (in_seq=3) asked for retransmission from the live
    // stream's seq 3, so the new leg must first replay the buffered frame
    // (the p_live2 payload, re-encrypted under the new nonce seq 5), and only
    // then forward fresh proxy data over connection B. Connection A must
    // receive nothing more (its stream was killed, its teardown skipped so
    // it could not fail/stop the new attachment).
    let retrans = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_raw_frame(&mut client_b),
    )
    .await
    .expect("recovered leg must retransmit the buffered frame")?;
    assert_eq!(frame_seq(&retrans), 5, "retransmission continues at seq 5");
    let mut client_b_check = Crypt::new(&material.key_send, 4);
    let (ch, payload) = decrypt_raw(&mut client_b_check, &retrans)?;
    assert_eq!(ch, 1);
    assert_eq!(
        payload, p_live2,
        "buffered live-stream frame is replayed first"
    );

    let p_new1 = vec![0xC3u8; 50];
    launcher_tx
        .send_async(PayloadWithChannel::new(1, &p_new1))
        .await?;
    let new_frame = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_raw_frame(&mut client_b),
    )
    .await
    .expect("new server stream must deliver proxy data over connection B")?;
    assert_eq!(frame_seq(&new_frame), 6, "new stream continues at seq 6");
    let (_, payload) = decrypt_raw(&mut client_b_check, &new_frame)?;
    assert_eq!(payload, p_new1);

    // The session's shared counters landed exactly where the wire shows:
    // inbound 3 (connect ticket echo 1 + recover confirm 2, each decrypt
    // advancing to last-used + 1), outbound 6 (live frames 2-3, OpenResponse
    // 4, retransmission 5, fresh payload 6 — every holder advanced the same
    // atomic).
    assert_eq!(session.seqs(), (3, 6));

    if let Ok(Ok(f)) = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        read_raw_frame(&mut client_a),
    )
    .await
    {
        panic!(
            "retired stream emitted seq={} after recovery",
            frame_seq(&f)
        );
    }

    // Keep the broker mock alive until the end of the test.
    drop(server);
    Ok(())
}
