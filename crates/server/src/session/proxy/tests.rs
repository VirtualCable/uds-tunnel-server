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

use std::os::unix::net::SocketAddr;

// Authors: Adolfo Gómez, dkmaster at dkmon dot com
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;

use shared::{
    crypt::{ticket, types::SharedSecret},
    ticket::Ticket,
};

const TEST_CHANNEL_ID: u16 = 1;

// Create an "remote server" for testing, and return the host:port, a stop trigger,
// a sender to send data to the server, and a receiver to get data from the server
async fn create_test_server() -> (
    String,
    Trigger,
    flume::Sender<Vec<u8>>,
    flume::Receiver<Vec<u8>>,
) {
    let stop = Trigger::new();
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = TcpListener::bind(&addr).await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let host_port = format!("{}:{}", local_addr.ip(), local_addr.port());
    let stop_clone = stop.clone();
    let (tx, rx) = flume::bounded(100);
    let (tx2, rx2) = flume::bounded::<Vec<u8>>(100);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_clone.wait_async() => {
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, _)) => {
                            let tx_clone = tx.clone();
                            let rx2 = rx2.clone();
                            let stop_inner = stop_clone.clone();
                            tokio::spawn(async move {
                                let (mut reader, mut writer) = stream.into_split();
                                let mut buf = vec![0u8; 1024];
                                loop {
                                    tokio::select! {
                                        _ = stop_inner.wait_async() => {
                                            log::debug!("Test server stopping");
                                            break;
                                        }
                                        result = reader.read(&mut buf) => {
                                            match result {
                                                Ok(0) => break, // Connection closed
                                                Ok(n) => {
                                                    log::debug!("Test server received {} bytes", n);
                                                    tx_clone.send_async(buf[..n].to_vec()).await.unwrap();
                                                }
                                                Err(e) => {
                                                    log::error!("Error reading from stream: {}", e);
                                                    break;
                                                }
                                            }
                                        }
                                        msg = rx2.recv_async() => {
                                            match msg {
                                                Ok(data) => {
                                                    log::debug!("Test server sending {} bytes", data.len());
                                                    if let Err(e) = writer.write_all(&data).await {
                                                        log::error!("Error writing to stream: {}", e);
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    log::error!("Error receiving from channel: {}", e);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            log::error!("Error accepting connection: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    });
    (host_port, stop, tx2, rx)
}

async fn wait_for_session_existence(session_id: &SessionId, must_exists: bool) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let exists = SessionManager::get_instance()
                .get_session(session_id)
                .is_some();
            if exists == must_exists {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await?;
    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn attach_detach_basic() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.start_server().await?;

    assert!(!server.tx.is_disconnected());

    handle.stop_server().await?; // Will end proxy, and in turn, the session 

    // Should trigger stop, check on a timeout to avoid test lock
    assert!(
        stop.wait_timeout_async(std::time::Duration::from_secs(2))
            .await
    );
    Ok(())
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn messages_preserve_order() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let (host_port, stop_server, _server_tx, server_rx) = create_test_server().await;

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let session = SessionManager::get_instance().add_session(Session::new(
        SharedSecret::new([0u8; 32]),
        Ticket::new_random(),
        stop.clone(),
        "172.27.0.1:1234".parse().unwrap(),
        vec![host_port],
    ))?;
    let _task = proxy.run(*session.id());

    let server = handle.start_server().await?;
    // Send open channel command
    server
        .tx
        .send_async((
            0,
            Command::OpenChannel {
                channel_id: TEST_CHANNEL_ID,
            }
            .to_bytes(),
        ))
        .await?;

    let count = 1000;

    for i in 0u32..count {
        server
            .tx
            .send_async((TEST_CHANNEL_ID, i.to_be_bytes().to_vec()))
            .await?;
    }

    // read and verify order
    // Note that this channel can contain more than 1 message per recv, so we need to handle that
    let mut received = 0u32;
    while received < count {
        let data = server_rx.recv_async().await?;
        for chunk in data.chunks(4) {
            let value = u32::from_be_bytes(chunk.try_into().unwrap());
            assert_eq!(value, received);
            received += 1;
        }
    }
    assert_eq!(received, count);

    // Stop TCP server, will close the connection and the proxy should close the client
    // But we do not have a method to check that, so we just stop everything

    // (So this part is only for debugginb purposes better than actual test)
    stop_server.trigger();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Stop proxy
    stop.trigger();
    // Session should be removed
    wait_for_session_existence(session.id(), false).await?;
    Ok(())
}

#[tokio::test]
async fn buffer_size_works() -> Result<()> {
    use crate::consts::CHANNEL_SIZE;

    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.start_server().await?;
    // No client, will cause buffer to fill up
    // Send until full buffer
    for _ in 0..(CHANNEL_SIZE) {
        server.tx.try_send((TEST_CHANNEL_ID, vec![1, 2, 3]))?;
    }
    // No fail yet

    // Next send should fail
    if let Err(e) = server.tx.try_send((TEST_CHANNEL_ID, vec![1, 2, 3])) {
        log::info!("Expected error on full buffer: {}", e);
    } else {
        panic!("Expected error on full buffer");
    }

    // No panic, no deadlock
    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn reattach_server_works() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);
    let (host_port, stop_server, server_tx, server_rx) = create_test_server().await;

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let session = SessionManager::get_instance().add_session(Session::new(
        SharedSecret::new([0u8; 32]),
        Ticket::new_random(),
        stop.clone(),
        "172.27.0.1:1234".parse().unwrap(),
        vec![host_port],
    ))?;
    let _task = proxy.run(*session.id());

    let server1 = handle.start_server().await?;

    // Send open channel command
    server1
        .tx
        .send_async((
            0,
            Command::OpenChannel {
                channel_id: TEST_CHANNEL_ID,
            }
            .to_bytes(),
        ))
        .await?;

    // let client = handle.attach_client(TEST_CHANNEL_ID).await?;
    server1
        .tx
        .send_async((TEST_CHANNEL_ID, b"first".to_vec()))
        .await?;
    let msg = server_rx.recv_async().await?;
    assert_eq!(msg, b"first");

    server_tx.send_async(b"from server".to_vec()).await?;
    let (chan, msg) = server1.rx.recv_async().await?;
    assert_eq!(&msg, b"from server");
    assert_eq!(chan, TEST_CHANNEL_ID);

    // Fail server will allow us to reattach
    handle.fail_server().await?;

    let server2 = handle.start_server().await?;
    server2
        .tx
        .send_async((TEST_CHANNEL_ID, b"second".to_vec()))
        .await?;
    let msg = server_rx.recv_async().await?;
    assert_eq!(msg, b"second");

    assert!(!stop.is_triggered());

    server_tx.send_async(b"from server".to_vec()).await?;
    let (chan, msg) = server2.rx.recv_async().await?;
    assert_eq!(&msg, b"from server");
    assert_eq!(chan, TEST_CHANNEL_ID);

    // Close server will trigger stop as server is gone
    handle.stop_server().await?;
    assert!(
        stop.wait_timeout_async(std::time::Duration::from_secs(1))
            .await
    );

    // Session should be removed
    wait_for_session_existence(session.id(), false).await?;

    // Stop our test server
    stop_server.trigger();

    Ok(())
}
