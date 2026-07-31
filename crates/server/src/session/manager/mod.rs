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

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock, RwLock};

use anyhow::Result;
use shared::log;
use shared::protocol::{PayloadWithChannelReceiver, PayloadWithChannelSender};

use crate::config;
use crate::session::SessionRecoveryBuffer;

use super::{Session, SessionId};

mod consts;

pub static SESSION_MANAGER: OnceLock<SessionManager> = OnceLock::new();

pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
}

impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions = self.sessions.read().unwrap();
        f.debug_struct("SessionManager")
            .field("sessions_count", &sessions.len())
            .finish()
    }
}

impl SessionManager {
    // New is private, use get_session_manager instead
    fn new() -> Self {
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Register a freshly built session.
    ///
    /// Rejects the insertion when the manager is already holding
    /// `config::ServerConfig::max_sessions()` entries (default 8192).
    /// Capping the active set keeps the O(n) lookup paths in
    /// `get_equiv_session` / `remove_equiv_session` bounded even
    /// under session-flood conditions.
    pub fn add_session(&self, session: Session) -> Result<Arc<Session>> {
        let max = config::get().read().unwrap().max_sessions();
        let mut sessions = self.sessions.write().unwrap();
        if sessions.len() >= max {
            log::warn!(
                "Refusing new session: SessionManager at cap ({} of {})",
                sessions.len(),
                max
            );
            // Drop the local `Session` here so its Drop impl triggers
            // the proxy shutdown; the proxy's exit path will call
            // `remove_session(&self)` with an id that is not in the
            // map, which is a no-op under the `if let Some` guard.
            return Err(anyhow::anyhow!(
                "SessionManager at cap ({} sessions, max {})",
                sessions.len(),
                max
            ));
        }
        let session = Arc::new(session);
        sessions.insert(session.id, session.clone());
        Ok(session)
    }

    pub fn get_session(&self, id: &SessionId) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().unwrap();
        sessions.get(id).cloned()
    }

    pub fn remove_session(&self, id: &SessionId) {
        let mut sessions = self.sessions.write().unwrap();
        if let Some(session) = sessions.get(id) {
            session.stop.trigger();
            sessions.remove(id);
        }
        // The session's `current_equiv_id` lives inside the Session and
        // is dropped together with the Arc, so no global equiv map needs
        // to be cleaned up here.
    }

    pub async fn finish_all_sessions(&self) {
        // Just drop session, will set the stop trigger
        let mut sessions = self.sessions.write().unwrap();
        sessions.clear();
    }

    pub fn count(&self) -> usize {
        let sessions = self.sessions.read().unwrap();
        sessions.len()
    }

    /// Effective cap from `ServerConfig::max_sessions()`. Convenience
    /// for tests and for the connection layer when logging rejections.
    pub fn max_sessions(&self) -> usize {
        config::get().read().unwrap().max_sessions()
    }

    /// Number of currently registered sessions whose `src_ip` matches
    /// the given remote. O(N) — only call when a per-IP cap is
    /// configured, otherwise the global `add_session` cap already
    /// bounds the active set.
    pub fn count_by_remote(&self, remote: std::net::SocketAddr) -> usize {
        let sessions = self.sessions.read().unwrap();
        sessions
            .values()
            .filter(|s| s.src_ip() == remote)
            .count()
    }

    pub async fn start_server(&self, id: &SessionId) -> Result<()> {
        if let Some(session) = self.get_session(id) {
            session.start_server().await?;
        }
        Ok(())
    }

    pub async fn stop_server(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            log::debug!("Stopping session {:?} server side", id);
            session.stop_server().await;
        }
    }

    pub async fn fail_server(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            log::debug!("Failing session {:?} server side", id);
            session.fail_server().await;
        }
    }

    pub async fn stop_client(&self, id: &SessionId, stream_channel_id: u16) {
        if let Some(session) = self.get_session(id) {
            log::debug!("Stopping session {:?} client side", id);
            session.stop_client(stream_channel_id).await;
        }
    }

    /// Look up a session by its external (equiv) id. Only the equiv id
    /// that the client knows is accepted; the internal session id is
    /// never exposed and is not a valid key.
    pub fn get_equiv_session(&self, id: &SessionId) -> Option<Arc<Session>> {
        let sessions = self.sessions.read().unwrap();
        sessions
            .values()
            .find(|s| s.current_equiv_id().as_ref() == Some(id))
            .cloned()
    }

    /// Mint a new random equiv id for the given session, replacing any
    /// previous one. The old equiv id becomes unresolvable the moment
    /// the new one is installed.
    pub fn create_equiv_session(&self, to: &SessionId) -> Result<SessionId> {
        let from = SessionId::new_random();
        let session = self
            .get_session(to)
            .ok_or_else(|| anyhow::anyhow!("Session {:?} not found", to))?;
        session.set_current_equiv_id(Some(from));
        log::debug!("Created equivalent session {:?} for {:?}", from, to);
        Ok(from)
    }

    /// Clear the equiv id of whichever session currently holds it, if
    /// any. Used by `recover::recover` to invalidate the inbound
    /// recover_session_id before minting a new one.
    pub fn remove_equiv_session(&self, from: &SessionId) {
        let sessions = self.sessions.read().unwrap();
        if let Some(session) = sessions
            .values()
            .find(|s| s.current_equiv_id().as_ref() == Some(from))
        {
            log::debug!("Removing equivalent session {:?} from manager", from);
            session.set_current_equiv_id(None);
        }
    }

    pub fn get_recovery_buffer(&self, id: &SessionId) -> Result<SessionRecoveryBuffer> {
        if let Some(session) = self.get_session(id) {
            Ok(session.recovery_buffer())
        } else {
            Err(anyhow::anyhow!("Session not found for recovery buffer"))
        }
    }

    pub fn is_close_notified(&self, id: &SessionId) -> bool {
        if let Some(session) = self.get_session(id) {
            session.is_close_notified()
        } else {
            true // If no session, session is close
        }
    }

    pub fn close_notified(&self, id: &SessionId) {
        if let Some(session) = self.get_session(id) {
            session.close_notified();
        }
    }

    pub fn get_server_channels(
        &self,
        id: &SessionId,
    ) -> Result<(PayloadWithChannelSender, PayloadWithChannelReceiver)> {
        if let Some(session) = self.get_session(id) {
            Ok(session.get_server_channels())
        } else {
            Err(anyhow::anyhow!("Session not found"))
        }
    }

    pub fn get_proxy_channels(
        &self,
        id: &SessionId,
    ) -> Result<(PayloadWithChannelSender, PayloadWithChannelReceiver)> {
        if let Some(session) = self.get_session(id) {
            Ok(session.get_proxy_channels())
        } else {
            Err(anyhow::anyhow!("Session not found"))
        }
    }

    // Get the global session manager instance
    pub fn get_instance() -> &'static SessionManager {
        SESSION_MANAGER.get_or_init(SessionManager::new)
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
