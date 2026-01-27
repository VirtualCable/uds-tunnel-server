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

use shared::{crypt::types::SharedSecret, system::trigger::Trigger, ticket};

fn new_session_for_test() -> Session {
    Session::new(
        SharedSecret::new([0u8; 32]),
        ticket::Ticket::new_random(),
        Trigger::new(),
        "127.0.0.1:0".parse().unwrap(),
    )
}

#[tokio::test]
async fn test_session_manager_add_and_get() {
    let manager = SessionManager::new();
    let (_session_id, session) = manager.add_session(new_session_for_test()).unwrap();
    // Fail if session is not found
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    assert!(!session.is_server_running());
    assert!(!session.is_client_running());
}

#[tokio::test]
async fn test_session_running() {
    let session = new_session_for_test();
    session.start_server().await.unwrap();
    assert!(session.is_server_running());
    assert!(!session.is_client_running());
}

#[tokio::test]
async fn test_session_sequence_numbers() {
    let session = new_session_for_test();
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
    let manager = SessionManager::get_instance();
    assert!(manager.sessions.read().unwrap().is_empty());
    // Clear the session manager for testing
    manager.sessions.write().unwrap().clear();
    let (_session_id, session) = manager.add_session(new_session_for_test()).unwrap();
    assert_eq!(*session.shared_secret().as_ref(), [0u8; 32]);
    // Clean up after test for other tests
}

#[tokio::test]
async fn test_session_lifecycle() {
    let manager = SessionManager::new();
    fn test_state(
        manager: &SessionManager,
        session_id: &SessionId,
        expected_server_is_running: bool,
        expected_client_is_running: bool,
    ) {
        let sess = manager.get_session(session_id).unwrap();
        assert_eq!(sess.is_server_running(), expected_server_is_running);
        assert_eq!(sess.is_client_running(), expected_client_is_running);
    }
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();
    manager.start_server(&session_id).await.unwrap();
    test_state(&manager, &session_id, true, false);
    manager.start_client(&session_id).await.unwrap();
    test_state(&manager, &session_id, true, true);
    manager.stop_server(&session_id).await.unwrap();
    test_state(&manager, &session_id, false, true);
    manager.stop_client(&session_id, 1).await.unwrap();
    assert!(manager.get_session(&session_id).is_none());
}

#[tokio::test]
async fn test_session_removed_exactly_once() {
    let manager = SessionManager::new();

    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();
    // Start servers first
    manager.start_server(&session_id).await.unwrap();
    manager.start_client(&session_id).await.unwrap();

    manager.stop_server(&session_id).await.unwrap();
    assert!(manager.get_session(&session_id).is_some());

    manager.stop_client(&session_id, 1).await.unwrap();
    assert!(manager.get_session(&session_id).is_none());

    // Any aditional stops should be no-ops
    manager.stop_server(&session_id).await.unwrap();
    manager.stop_client(&session_id, 1).await.unwrap();
}

#[tokio::test]
async fn test_get_session_returns_arc_clone() {
    let manager = SessionManager::new();
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();

    let s1 = manager.get_session(&session_id).unwrap();
    let s2 = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&s1, &s2));
}

#[tokio::test]
async fn test_get_equiv_session_default() {
    let manager = SessionManager::new();
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();

    let equiv_session = manager.get_equiv_session(&session_id).unwrap();
    let direct_session = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_add_equiv_session() {
    let manager = SessionManager::new();
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();
    let equiv_session = manager.get_equiv_session(&equiv_session_id).unwrap();
    let direct_session = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_remove_session_removes_equiv_session() {
    let manager = SessionManager::new();
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();

    manager.remove_session(&session_id);

    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
    assert!(manager.get_session(&session_id).is_none());
}

#[tokio::test]
async fn test_remove_equiv_session() {
    let manager = SessionManager::new();
    let (session_id, _) = manager.add_session(new_session_for_test()).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();

    manager.remove_equiv_session(&equiv_session_id);

    // Original session should still exist
    assert!(manager.get_session(&session_id).is_some());
    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
}
