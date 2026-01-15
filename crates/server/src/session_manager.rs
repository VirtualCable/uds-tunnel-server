use flume::{Receiver, Sender};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use crate::{consts::TICKET_LENGTH, system::trigger::Trigger};

pub static SESSION_MANAGER: OnceLock<SessionManager> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; TICKET_LENGTH]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningState {
    Running,
    Stopped,
}

pub struct Session {
    pub shared_secret: [u8; 32],
    pub server_to_client: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
    pub client_to_server: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
    pub stop: Trigger,
    // Server is the side that accepted the connection from the client
    pub server_state: RunningState,
    // Client is the side that initiated the connection to the remote server
    pub client_state: RunningState,
    // seq numbers for crypto part
    // only updated on server side killed.
    pub seq_tx: u64,
    pub seq_rx: u64,
}

impl Session {
    pub fn new(
        shared_secret: [u8; 32],
        server_to_client: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
        client_to_server: (Sender<Vec<u8>>, Receiver<Vec<u8>>),
        stop: Trigger,
    ) -> Self {
        Session {
            shared_secret,
            server_to_client,
            client_to_server,
            stop,
            server_state: RunningState::Stopped,
            client_state: RunningState::Stopped,
            seq_tx: 0,
            seq_rx: 0,
        }
    }

}

pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<RwLock<Session>>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_session(&self, id: SessionId, session: Session) {
        let mut sessions = self.sessions.write().unwrap();
        sessions.insert(id, Arc::new(RwLock::new(session)));
    }

    pub fn get_session(&self, id: &SessionId) -> Option<Arc<RwLock<Session>>> {
        let sessions = self.sessions.read().unwrap();
        sessions.get(id).cloned()
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_manager_add_and_get() {
        let manager = SessionManager::new();
        let session_id = SessionId([0u8; TICKET_LENGTH]);
        let session = Session::new(
            [0u8; 32],
            (flume::unbounded().0, flume::unbounded().1),
            (flume::unbounded().0, flume::unbounded().1),
            Trigger::new(),
        );
        manager.add_session(session_id, session);
        // Fail if session is not found
        let retrieved_session = manager.get_session(&session_id).unwrap();
        assert_eq!(retrieved_session.read().unwrap().shared_secret, [0u8; 32]);
        assert_eq!(
            retrieved_session.read().unwrap().server_state,
            RunningState::Stopped
        );
        assert_eq!(
            retrieved_session.read().unwrap().client_state,
            RunningState::Stopped
        );
    }

    #[test]
    fn test_session_state_management() {
        let mut session = Session::new(
            [0u8; 32],
            (flume::unbounded().0, flume::unbounded().1),
            (flume::unbounded().0, flume::unbounded().1),
            Trigger::new(),
        );
        session.server_state = RunningState::Running;
        session.client_state = RunningState::Running;
        assert_eq!(session.server_state, RunningState::Running);
        assert_eq!(session.client_state, RunningState::Running);
    }

    #[test]
    fn test_session_sequence_numbers() {
        let mut session = Session::new(
            [0u8; 32],
            (flume::unbounded().0, flume::unbounded().1),
            (flume::unbounded().0, flume::unbounded().1),
            Trigger::new(),
        );
        assert_eq!(session.seq_tx, 0);
        assert_eq!(session.seq_rx, 0);
        session.seq_tx += 1;
        session.seq_rx += 2;
        assert_eq!(session.seq_tx, 1);
        assert_eq!(session.seq_rx, 2);
    }
}
