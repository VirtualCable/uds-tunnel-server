use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Session, SessionId};

mod consts;

pub static SESSION_MANAGER: OnceLock<SessionManager> = OnceLock::new();

pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
    // For equivalent sessions mapping
    equivs: RwLock<HashMap<SessionId, (SessionId, Instant)>>,
    last_cleanup: RwLock<Instant>,
}

impl SessionManager {
    // New is private, use get_session_manager instead
    fn new() -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
            equivs: RwLock::new(HashMap::new()),
            last_cleanup: RwLock::new(Instant::now()),
        }
    }

    // A new session is created with a new session id
    // Also add an idempotent entry in equivs, so wen recovering from this session id works
    // Without no more checks
    pub fn add_session(&self, session: Session) -> Result<SessionId> {
        let session_id = SessionId::new();
        {
            let mut sessions = self.sessions.write().unwrap();
            sessions.insert(session_id, Arc::new(session));
        }
        // Also, insert an idempotent entry in equivs
        {
            let mut equivs = self.equivs.write().unwrap();
            equivs.insert(session_id, (session_id, Instant::now()));
        }
        Ok(session_id)
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

    /// Note: equivs will fail if the target session is removed or the equiv entry does not exist
    pub fn get_equiv_session(&self, id: &SessionId) -> Option<Arc<Session>> {
        // If equivalent session exists, get it. If don't, try to use id as is.

        // Ensure lock scope is limited
        let equivs = self.equivs.read().unwrap();
        if let Some(equiv_id) = equivs.get(id) {
            self.get_session(&equiv_id.0)
        } else {
            None
        }
    }

    pub fn create_equiv_session(&self, to: SessionId) -> Result<SessionId> {
        // If too many entries, return err
        {
            let equivs = self.equivs.read().unwrap();
            if equivs.len() >= consts::MAX_EQUIV_ENTRIES {
                anyhow::bail!("Too many equivalent session entries");
            }
        }
        let from = SessionId::new();
        let mut equivs = self.equivs.write().unwrap();
        equivs.insert(from, (to, Instant::now()));
        Ok(from)
    }

    pub fn remove_equiv_session(&self, from: &SessionId) {
        let mut equivs = self.equivs.write().unwrap();
        equivs.remove(from);
    }

    pub fn cleanup_equiv_sessions(&self, max_age: std::time::Duration) {
        let mut equivs = self.equivs.write().unwrap();
        let now = Instant::now();
        equivs.retain(|_, (_, timestamp)| now.duration_since(*timestamp) < max_age);
    }

    fn maybe_cleanup_equivs(&self) {
        let now = Instant::now();
        let mut last = self.last_cleanup.write().unwrap();
        if now.duration_since(*last)
            > Duration::from_secs(consts::CLEANUP_EQUIV_SESSIONS_INTERVAL_SECS)
        {
            self.cleanup_equiv_sessions(Duration::from_secs(consts::EQUIV_SESSION_MAX_AGE_SECS));
            *last = now;
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
    let manager = SESSION_MANAGER.get_or_init(SessionManager::new);
    manager.maybe_cleanup_equivs(); // Lazy cleanup on each access
    manager
}

#[cfg(test)]
mod tests;
