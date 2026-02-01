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

const TEST_CHANNEL_ID: u16 = 1;

#[serial_test::serial(manager)]
#[tokio::test]
async fn attach_detach_basic() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.attach_server().await?;
    let client = handle.attach_client(TEST_CHANNEL_ID).await?;

    assert!(!server.tx.is_disconnected());
    assert!(!client.tx.is_disconnected());

    handle.detach_server().await?;

    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn messages_preserve_order() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.attach_server().await?;
    let client = handle.attach_client(TEST_CHANNEL_ID).await?;

    let count = 1000;

    for i in 0u32..count {
        server
            .tx
            .send_async((TEST_CHANNEL_ID, i.to_be_bytes().to_vec()))
            .await?;
    }

    for i in 0..count {
        let msg = client.rx.recv_async().await?;
        let num = u32::from_be_bytes(msg.try_into().unwrap());
        assert_eq!(num, i);
    }

    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn backpressure_does_not_panic() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.attach_server().await?;
    let _client = handle.attach_client(TEST_CHANNEL_ID).await?;

    // CHANNEL_SIZE = 1 → backpressure real
    for _ in 0..10 {
        let _ = server.tx.send_async((TEST_CHANNEL_ID, vec![1, 2, 3])).await;
    }

    // No panic, no deadlock
    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn closes_on_full_buffer() -> Result<()> {
    use crate::consts::CHANNEL_SIZE;

    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.attach_server().await?;
    // No client, will cause buffer to fill up

    for _ in 0..(CHANNEL_SIZE) {
        server.tx.try_send((TEST_CHANNEL_ID, vec![1, 2, 3]))?;
    }
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

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server1 = handle.attach_server().await?;
    let client = handle.attach_client(TEST_CHANNEL_ID).await?;
    server1
        .tx
        .send_async((TEST_CHANNEL_ID, b"first".to_vec()))
        .await?;
    let msg = client.rx.recv_async().await?;
    assert_eq!(msg, b"first");

    handle.detach_server().await?;

    let server2 = handle.attach_server().await?;
    server2
        .tx
        .send_async((TEST_CHANNEL_ID, b"second".to_vec()))
        .await?;
    let msg = client.rx.recv_async().await?;
    assert_eq!(msg, b"second");

    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn fairness_between_sides() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _task = proxy.run(SessionId::new_random());

    let server = handle.attach_server().await?;
    let client = handle.attach_client(TEST_CHANNEL_ID).await?;

    for i in 0..100 {
        server.tx.send_async((TEST_CHANNEL_ID, vec![i])).await?;
        client.tx.send_async((TEST_CHANNEL_ID, vec![i])).await?;
    }

    for _ in 0..100 {
        let _ = server.rx.recv_async().await?;
        let _ = client.rx.recv_async().await?;
    }

    stop.trigger();
    Ok(())
}

#[tokio::test]
async fn test_proxy_communication() -> Result<()> {
    log::setup_logging("debug", log::LogType::Test);

    log::info!("Starting test_proxy_communication");

    let stop = Trigger::new();
    let (proxy, handle) = Proxy::new(stop.clone());
    let _proxy_task = proxy.run(SessionId::new_random());

    let server_endpoints = handle.attach_server().await?;
    let client_endpoints = handle.attach_client(TEST_CHANNEL_ID).await?;

    let test_message = b"Hello, Proxy!".to_vec();

    // Test the timeout of tokio to ensure it works
    let res = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        log::debug!("Waiting to receive message on server side");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    })
    .await;
    log::debug!("Timeout test passed: {:?}", res);

    // Send message from server to client, with a timeout to avoid hanging test
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        server_endpoints
            .tx
            .send_async((TEST_CHANNEL_ID, test_message.clone()))
            .await
    })
    .await??;
    // Receive message from client, with a timeout to avoid hanging test
    let received_by_client = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        log::debug!("Waiting to receive message on client side");
        let res = client_endpoints.rx.recv_async().await;
        log::debug!("Received message on client side");
        res
    })
    .await??;
    assert_eq!(received_by_client, test_message);
    log::debug!("Message from server to client verified");

    // Send message from client to server, with a timeout to avoid hanging test
    let response_message = b"Hello, Server!".to_vec();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        client_endpoints
            .tx
            .send_async((TEST_CHANNEL_ID, response_message.clone()))
            .await
    })
    .await??;
    let received_by_server = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        server_endpoints.rx.recv_async().await
    })
    .await??;

    assert_eq!(received_by_server.0, TEST_CHANNEL_ID);
    assert_eq!(received_by_server.1, response_message);
    log::debug!("Message from client to server verified");

    stop.trigger();

    Ok(())
}
