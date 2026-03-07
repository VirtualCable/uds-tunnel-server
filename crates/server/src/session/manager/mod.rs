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
use std::time::{Duration, Instant};

use anyhow::Result;
use shared::log;
use shared::protocol::{PayloadWithChannelReceiver, PayloadWithChannelSender};

use crate::session::SessionRecoveryBuffer;

use super::{Session, SessionId};

mod consts;

pub static SESSION_MANAGER: OnceLock<SessionManager> = OnceLock::new();

pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Session>>>,
    // For equivalent sessions mapping
    equivs: RwLock<HashMap<SessionId, SessionId>>,
    last_cleanup: RwLock<Instant>,
}

impl fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sessions = self.sessions.read().unwrap();
        let equivs = self.equivs.read().unwrap();
        f.debug_struct("SessionManager")
            .field("sessions_count", &sessions.len())
            .field("equivs_count", &equivs.len())
            .finish()
    }
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
    pub fn add_session(&self, session: Session) -> Result<Arc<Session>> {
        let session = {
            let mut sessions = self.sessions.write().unwrap();
            let session = Arc::new(session);
            sessions.insert(session.id, session.clone());
            session
        };
        // Also, insert an idempotent entry in equivs
        {
            let mut equivs = self.equivs.write().unwrap();
            equivs.insert(session.id, session.id);
        }
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

    /// Note: equivs will fail if the target session is removed or the equiv entry does not exist
    pub fn get_equiv_session(&self, id: &SessionId) -> Option<Arc<Session>> {
        // If equivalent session exists, get it. If don't, try to use id as is.

        // Ensure lock scope is limited
        let equivs = self.equivs.read().unwrap();
        if let Some(equiv_id) = equivs.get(id) {
            self.get_session(equiv_id)
        } else {
            None
        }
    }

    pub fn create_equiv_session(&self, to: &SessionId) -> Result<SessionId> {
        // If too many entries, return err
        {
            let equivs = self.equivs.read().unwrap();
            if equivs.len() >= consts::MAX_EQUIV_ENTRIES {
                anyhow::bail!("Too many equivalent session entries");
            }
        }
        let from = SessionId::new_random();
        let mut equivs = self.equivs.write().unwrap();
        equivs.insert(from, *to);
        log::debug!("Created equivalent session {:?} from {:?}", from, to);
        Ok(from)
    }

    pub fn remove_equiv_session(&self, from: &SessionId) {
        log::debug!("Removing equivalent session {:?} from manager", from);
        let mut equivs = self.equivs.write().unwrap();
        equivs.remove(from);
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

    fn cleanup_equiv_sessions(&self) {
        let mut equivs = self.equivs.write().unwrap();
        // Remove entries that are too old and original session does not exist anymore
        equivs.retain(|_, orig| self.get_session(orig).is_some());
    }

    // Lazy cleanup of equiv sessions on each access, to avoid needing a background task
    // This allows to easyly keep memory in limits.
    fn maybe_cleanup_equivs(&self) {
        let now = Instant::now();
        let mut last = self.last_cleanup.write().unwrap();
        if now.duration_since(*last)
            > Duration::from_secs(consts::CLEANUP_EQUIV_SESSIONS_INTERVAL_SECS)
        {
            self.cleanup_equiv_sessions();
            *last = now;
        }
    }

    // Get the global session manager instance
    pub fn get_instance() -> &'static SessionManager {
        let manager = SESSION_MANAGER.get_or_init(SessionManager::new);
        manager.maybe_cleanup_equivs(); // Lazy cleanup on each access
        manager
    }

    pub fn log_debug_sessions(&self) {
        let sessions = self.sessions.read().unwrap();
        let equivs = self.equivs.read().unwrap();
        log::debug!("Sessions: {:?}", sessions);
        log::debug!("Equivs: {:?}", equivs);
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
