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

use shared::{crypt::types::SharedSecret, log, system::trigger::Trigger, ticket};

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

fn new_session_for_test(remote: &str) -> Session {
    Session::new(
        SharedSecret::new([0u8; 32]),
        ticket::Ticket::new_random(),
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
        vec![remote.to_string()],
    )
}

#[tokio::test]
async fn test_session_manager_add_and_get() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    // Fail if session is not found
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    assert!(!session.is_server_running());
    assert!(session.is_running()); // Proxy should be running by default
    assert!(manager.get_session(session.id()).is_some());
    assert_eq!(manager.count(), 1);
}

#[tokio::test]
async fn test_session_running() {
    log::setup_logging("debug", log::LogType::Test);

    let session = new_session_for_test("127.0.0.1:1234");
    session.start_server().await.unwrap();
    assert!(session.is_running());
    assert!(session.is_server_running());
}

#[tokio::test]
async fn test_session_sequence_numbers() {
    log::setup_logging("debug", log::LogType::Test);

    let session = new_session_for_test("127.0.0.1:1234");
    let seq = session.seqs();
    assert_eq!(seq, (0, 0));
    session.set_inbound_seq(5);
    session.set_outbound_seq(10);
    let seq = session.seqs();
    assert_eq!(seq, (5, 10));
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_get_session_manager() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();
    assert!(manager.sessions.read().unwrap().is_empty());
    // Clear the session manager for testing
    manager.sessions.write().unwrap().clear();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    // Clean up after test for other tests
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_session_lifecycle() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();

    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    manager.start_server(session.id()).await.unwrap();
    assert!(session.is_running());
    assert!(session.is_server_running());
    assert!(manager.get_session(session.id()).is_some());

    manager.stop_server(session.id()).await;
    assert!(
        session
            .stop
            .wait_timeout_async(std::time::Duration::from_millis(500))
            .await
    );
    assert!(!session.is_server_running());
    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();

    // No client is running in fact, and as the proxy is stopped,
    // but this should not fail
    manager.stop_client(session.id(), 1).await;
    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();
}

#[serial_test::serial(manager)]
#[tokio::test]
async fn test_session_removed_exactly_once() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::get_instance();

    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();
    // Start servers first
    manager.start_server(session.id()).await.unwrap();
    assert!(manager.get_session(session.id()).is_some());

    manager.stop_server(session.id()).await;

    wait_for_session_existence(session.id(), false)
        .await
        .unwrap();

    // Any aditional stops should be no-ops
    manager.stop_server(session.id()).await;
    manager.stop_client(session.id(), 1).await;
}

#[tokio::test]
async fn test_get_session_returns_arc_clone() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let s1 = manager.get_session(session.id()).unwrap();
    let s2 = manager.get_session(session.id()).unwrap();

    assert!(Arc::ptr_eq(&s1, &s2));
}

#[tokio::test]
async fn test_get_equiv_session_default() {
    log::setup_logging("debug", log::LogType::Test);

    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session = manager.get_equiv_session(session.id()).unwrap();
    let direct_session = manager.get_session(session.id()).unwrap();

    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_add_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();
    let equiv_session = manager.get_equiv_session(&equiv_session_id).unwrap();
    let direct_session = manager.get_session(session.id()).unwrap();
    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_remove_session_removes_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();
    manager.remove_session(session.id());

    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
    assert!(manager.get_session(session.id()).is_none());
}

#[tokio::test]
async fn test_remove_equiv_session() {
    let manager = SessionManager::new();
    let session = manager
        .add_session(new_session_for_test("127.0.0.1:1234"))
        .unwrap();

    let equiv_session_id = manager.create_equiv_session(session.id()).unwrap();

    manager.remove_equiv_session(&equiv_session_id);

    // Original session should still exist
    assert!(manager.get_session(session.id()).is_some());
    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
}
