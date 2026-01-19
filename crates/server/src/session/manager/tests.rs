use super::*;

use crate::{consts::TICKET_LENGTH, system::trigger::Trigger};

fn new_session_for_test() -> Session {
    let ticket = [0x40u8; TICKET_LENGTH];
    Session::new([0u8; 32], ticket, Trigger::new())
}

#[tokio::test]
async fn test_session_manager_add_and_get() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();
    // Fail if session is not found
    let retrieved_session = manager.get_session(&session_id).unwrap();
    assert_eq!(retrieved_session.get_shared_secret(), [0u8; 32]);
    assert!(!retrieved_session.is_server_running());
    assert!(!retrieved_session.is_client_running());
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
    let seq = session.get_seq();
    assert_eq!(seq, (0, 0));
    session.set_seq(5, 10);
    let seq = session.get_seq();
    assert_eq!(seq, (5, 10));
}

#[tokio::test]
async fn test_get_session_manager() {
    let manager = get_session_manager();
    assert!(manager.sessions.read().unwrap().is_empty());
    // Clear the session manager for testing
    manager.sessions.write().unwrap().clear();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();
    let retrieved_session = manager.get_session(&session_id).unwrap();
    assert_eq!(retrieved_session.get_shared_secret(), [0u8; 32]);
}

#[tokio::test]
async fn test_session_lifecycle() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
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
    let session_id = manager.add_session(session).unwrap();
    manager.start_server(&session_id).await.unwrap();
    test_state(&manager, &session_id, true, false);
    manager.start_client(&session_id).await.unwrap();
    test_state(&manager, &session_id, true, true);
    manager.stop_server(&session_id).await.unwrap();
    test_state(&manager, &session_id, false, true);
    manager.stop_client(&session_id).await.unwrap();
    assert!(manager.get_session(&session_id).is_none());
}

#[tokio::test]
async fn test_session_removed_exactly_once() {
    let manager = SessionManager::new();
    let session = new_session_for_test();

    let session_id = manager.add_session(session).unwrap();
    // Start servers first
    manager.start_server(&session_id).await.unwrap();
    manager.start_client(&session_id).await.unwrap();

    manager.stop_server(&session_id).await.unwrap();
    assert!(manager.get_session(&session_id).is_some());

    manager.stop_client(&session_id).await.unwrap();
    assert!(manager.get_session(&session_id).is_none());

    // Any aditional stops should be no-ops
    manager.stop_server(&session_id).await.unwrap();
    manager.stop_client(&session_id).await.unwrap();
}

#[tokio::test]
async fn test_concurrent_access() {
    use std::sync::Arc;

    let manager = Arc::new(SessionManager::new());
    let session = new_session_for_test();

    let session_id = manager.add_session(session).unwrap();

    let mut handles = vec![];

    for _ in 0..20 {
        let manager = manager.clone();
        handles.push(tokio::task::spawn(async move {
            for _ in 0..1000 {
                manager.start_server(&session_id).await.unwrap();
                manager.start_client(&session_id).await.unwrap();
                manager.store_sequence_numbers(&session_id, 10, 20);
                let _ = manager.get_sequence_numbers(&session_id);
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let seq = manager.get_sequence_numbers(&session_id);
    assert_eq!(seq, (10, 20));
}

#[tokio::test]
async fn test_sequence_consistency_under_concurrency() {
    use std::sync::Arc;
    use std::thread;

    let session = Arc::new(new_session_for_test());

    let mut handles = vec![];

    // Set initial sequence numbers to zero, 1
    session.set_seq(0, 1);

    // Writer
    {
        let session = session.clone();
        handles.push(thread::spawn(move || {
            for i in 0..1000 {
                session.set_seq(i, i + 1);
            }
        }));
    }

    // Readers
    for _ in 0..10 {
        let session = session.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..1000 {
                let (tx, rx) = session.get_seq();
                assert_eq!(rx, tx + 1);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

#[tokio::test]
async fn test_get_session_returns_arc_clone() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();

    let s1 = manager.get_session(&session_id).unwrap();
    let s2 = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&s1, &s2));
}

#[tokio::test]
async fn test_get_equiv_session_no_equiv() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();

    let equiv_session = manager.get_equiv_session(&session_id).unwrap();
    let direct_session = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_add_equiv_session() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();
    let equiv_session = manager.get_equiv_session(&equiv_session_id).unwrap();
    let direct_session = manager.get_session(&session_id).unwrap();

    assert!(Arc::ptr_eq(&equiv_session, &direct_session));
}

#[tokio::test]
async fn test_remove_session_removes_equiv_session() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();

    manager.remove_session(&session_id);

    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
    assert!(manager.get_session(&session_id).is_none());
}

#[tokio::test]
async fn test_remove_equiv_session() {
    let manager = SessionManager::new();
    let session = new_session_for_test();
    let session_id = manager.add_session(session).unwrap();

    let equiv_session_id = manager.create_equiv_session(session_id).unwrap();

    manager.remove_equiv_session(&equiv_session_id);

    // Original session should still exist
    assert!(manager.get_session(&session_id).is_some());
    assert!(manager.get_equiv_session(&equiv_session_id).is_none());
}
