use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::Result;

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

    pub async fn start_server(&self, id: &SessionId) -> Result<()> {
        if let Some(session) = self.get_session(id) {
            session.start_server().await?;
        }
        Ok(())
    }

    pub async fn stop_server(&self, id: &SessionId) -> Result<()> {
        if let Some(session) = self.get_session(id) {
            session.stop_server().await?;
            // Alreasy knows that server is not running
            if !session.is_client_running() {
                self.remove_session(id);
            }
        }
        Ok(())
    }

    // Client is the outgoing connection to the remote server
    // This side is not recoverable, so if it stops, the session is finished
    pub async fn start_client(&self, id: &SessionId) -> Result<()> {
        if let Some(session) = self.get_session(id) {
            session.start_client().await?;
        }
        Ok(())
    }

    pub async fn stop_client(&self, id: &SessionId) -> Result<()> {
        if let Some(session) = self.get_session(id) {
            session.stop_client().await?;
            // Remove the session. It's fine even if any check against
            // this session is done after this point.
            // Note: drop of session will invoke trigger stop
            self.remove_session(id);
        }
        Ok(())
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
mod tests;