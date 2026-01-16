use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use super::session_id::{Session, SessionId};

pub static SESSION_MANAGER: OnceLock<SessionManager> = OnceLock::new();

pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    // New is private, use get_session_manager instead
    fn new() -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_session(&self, id: SessionId, session: Session) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.insert(id, Arc::new(session));
    }

    pub fn get_session(&self, id: &SessionId) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().unwrap();
        sessions.get(id).cloned()
    }

    pub fn remove_session(&self, id: &SessionId) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.remove(id);
    }

    pub fn start_server(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            session.start_server();
        }
    }

    pub fn stop_server(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            session.stop_server();
            // Alreasy knows that server is not running
            if !session.is_client_running() {
                self.remove_session(id);
            }
        }
    }

    // Client is the outgoing connection to the remote server
    // This side is not recoverable, so if it stops, the session is finished
    pub fn start_client(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            session.start_client();
        }
    }

    pub fn stop_client(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            session.stop_client();
            // Remove the session. It's if fien even if any check against
            // this session is done after this point.
            // Note: drop of session will invoke trigger stop
            self.remove_session(id);
        }
    }

    pub fn store_sequence_numbers(&self, id: &SessionId, seq_tx: u64, seq_rx: u64) {
        if let Some(session) = self.get_session(id) {
            session.set_seq(seq_tx, seq_rx);
        }
    }

    pub fn get_sequence_numbers(&self, id: &SessionId) -> (u64, u64) {
        if let Some(session) = self.get_session(id) {
            session.get_seq()
        } else {
            (0, 0)
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// Get the global session manager instance
pub fn get_session_manager() -> &'static SessionManager {
    SESSION_MANAGER.get_or_init(SessionManager::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{consts::TICKET_LENGTH, system::trigger::Trigger};

    #[tokio::test]
    async fn test_session_manager_add_and_get() {
        let manager = SessionManager::new();
        let session_id = SessionId::new([0u8; TICKET_LENGTH]);
        let session = Session::new([0u8; 32], Trigger::new());
        manager.add_session(session_id, session);
        // Fail if session is not found
        let retrieved_session = manager.get_session(&session_id).unwrap();
        assert_eq!(retrieved_session.get_shared_secret(), [0u8; 32]);
        assert!(!retrieved_session.is_server_running());
        assert!(!retrieved_session.is_client_running());
    }

    #[tokio::test]
    async fn test_session_running() {
        let session = Session::new([0u8; 32], Trigger::new());
        session.start_server();
        assert!(session.is_server_running());
        assert!(!session.is_client_running());
    }

    #[tokio::test]
    async fn test_session_sequence_numbers() {
        let session = Session::new([0u8; 32], Trigger::new());
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
        let session_id = SessionId::new([0u8; TICKET_LENGTH]);
        let session = Session::new([0u8; 32], Trigger::new());
        manager.add_session(session_id, session);
        let retrieved_session = manager.get_session(&session_id).unwrap();
        assert_eq!(retrieved_session.get_shared_secret(), [0u8; 32]);
    }

    #[tokio::test]
    async fn test_session_lifecycle() {
        let manager = SessionManager::new();
        let session_id = SessionId::new([1u8; TICKET_LENGTH]);
        let session = Session::new([0u8; 32], Trigger::new());
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
        manager.add_session(session_id, session);
        manager.start_server(&session_id);
        test_state(&manager, &session_id, true, false);
        manager.start_client(&session_id);
        test_state(&manager, &session_id, true, true);
        manager.stop_server(&session_id);
        test_state(&manager, &session_id, false, true);
        manager.stop_client(&session_id);
        assert!(manager.get_session(&session_id).is_none());
    }

    #[tokio::test]
    async fn test_session_removed_exactly_once() {
        let manager = SessionManager::new();
        let id = SessionId::new([9u8; TICKET_LENGTH]);

        manager.add_session(id, Session::new([0u8; 32], Trigger::new()));
        // Start servers first
        manager.start_server(&id);
        manager.start_client(&id);

        manager.stop_server(&id);
        assert!(manager.get_session(&id).is_some());

        manager.stop_client(&id);
        assert!(manager.get_session(&id).is_none());

        // Any aditional stops should be no-ops
        manager.stop_server(&id);
        manager.stop_client(&id);
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(SessionManager::new());
        let session_id = SessionId::new([42u8; TICKET_LENGTH]);

        manager.add_session(session_id, Session::new([1u8; 32], Trigger::new()));

        let mut handles = vec![];

        for _ in 0..20 {
            let manager = manager.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    manager.start_server(&session_id);
                    manager.start_client(&session_id);
                    manager.store_sequence_numbers(&session_id, 10, 20);
                    let _ = manager.get_sequence_numbers(&session_id);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let seq = manager.get_sequence_numbers(&session_id);
        assert_eq!(seq, (10, 20));
    }

    #[tokio::test]
    async fn test_sequence_consistency_under_concurrency() {
        use std::sync::Arc;
        use std::thread;

        let session = Arc::new(Session::new([0u8; 32], Trigger::new()));

        let mut handles = vec![];

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
        let id = SessionId::new([7u8; TICKET_LENGTH]);

        manager.add_session(id, Session::new([0u8; 32], Trigger::new()));

        let s1 = manager.get_session(&id).unwrap();
        let s2 = manager.get_session(&id).unwrap();

        assert!(Arc::ptr_eq(&s1, &s2));
    }
}
