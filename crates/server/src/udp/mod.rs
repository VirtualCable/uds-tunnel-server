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

//! UDP relay leg of the tunnel.
//!
//! One shared Tokio `UdpSocket` receives datagrams from every launcher;
//! a per-session 128-bit token (`UdpToken`, carried in the clear as the
//! datagram header) demultiplexes to the owning `Session`. Each datagram
//! is AEAD-verified with the session's `DatagramCrypt` before anything is
//! forwarded, and the relay never *sends* to a client address that has
//! not previously produced a valid datagram (anti-amplification).
//!
//! Registration lifecycle lives in `SessionManager` (`add_session` /
//! `remove_session`), which calls [`register_session`] /
//! [`unregister_token`] here; both are no-ops when the relay is not
//! running (config `udp_enabled = false`, bind failure, or tests).

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use tokio::net::UdpSocket;

use shared::{
    crypt::datagram::{DATAGRAM_HEADER_SIZE, MAX_DATAGRAM_PAYLOAD, TOKEN_LENGTH, UdpToken},
    log,
    system::trigger::Trigger,
};

use crate::session::{Session, UdpState};

/// How often the relay sweeps for dead sessions and idle UDP legs.
const REAP_INTERVAL_SECS: u64 = 60;
/// UDP legs with no authenticated traffic for this long are torn down
/// (the TCP session survives; RDP simply keeps using the TCP leg).
pub const UDP_IDLE_TIMEOUT_SECS: u64 = 300;
/// How often a per-session return task re-checks that its UDP leg is
/// still installed on the session (the reaper may have cleared it).
const RETURN_ALIVE_CHECK_SECS: u64 = 10;

const TAG_LENGTH: usize = shared::crypt::consts::TAG_LENGTH;
/// Largest datagram we are willing to read off the wire.
const MAX_DATAGRAM_SIZE: usize = DATAGRAM_HEADER_SIZE + MAX_DATAGRAM_PAYLOAD + TAG_LENGTH;

#[derive(Debug, Default)]
struct Counters {
    received: AtomicU64,
    forwarded: AtomicU64, // client -> remote, after successful decrypt
    sent: AtomicU64,      // remote -> client
    discarded_short: AtomicU64,
    discarded_unknown: AtomicU64,        // unknown/dead token
    discarded_replay: AtomicU64,         // duplicate or outside replay window
    auth_fail: AtomicU64,                // malformed or AEAD verification failure
    discarded_foreign_source: AtomicU64, // authenticated but ip != tunnel peer
    discarded_leg_backoff: AtomicU64,    // leg creation pending, cooldown active
    not_sent_no_addr: AtomicU64,         // remote data with no authenticated client addr
}

impl Counters {
    fn bump(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct UdpRelay {
    socket: Arc<UdpSocket>,
    sessions: RwLock<HashMap<UdpToken, Weak<Session>>>,
    counters: Counters,
}

static UDP_RELAY: RwLock<Option<Arc<UdpRelay>>> = RwLock::new(None);

fn global_relay() -> Option<Arc<UdpRelay>> {
    UDP_RELAY.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Register a session's UDP token with the running relay. Called from
/// `SessionManager::add_session`; no-op when the relay is not running.
pub fn register_session(session: &Arc<Session>) {
    if let Some(relay) = global_relay() {
        relay.register(session);
    }
}

/// Remove a token from the relay map. Called from
/// `SessionManager::remove_session` and the relay reaper.
pub fn unregister_token(token: &UdpToken) {
    if let Some(relay) = global_relay() {
        relay.unregister(token);
    }
}

impl UdpRelay {
    /// Bind the shared socket and install the relay as the global
    /// instance, replacing any previous one (production binds exactly
    /// once from `main`; tests rebind to point the `SessionManager`
    /// registration hook at their own relay). Does not spawn the
    /// receive loop; call [`UdpRelay::run`].
    pub async fn bind(addr: SocketAddr) -> Result<Arc<Self>> {
        let socket = UdpSocket::bind(addr).await?;
        let relay = Arc::new(UdpRelay {
            socket: Arc::new(socket),
            sessions: RwLock::new(HashMap::new()),
            counters: Counters::default(),
        });
        *UDP_RELAY.write().unwrap_or_else(|e| e.into_inner()) = Some(relay.clone());
        Ok(relay)
    }

    /// Test-only: bind a relay WITHOUT installing it as the process-global
    /// instance, so unit tests drive their own relay directly and do not
    /// race each other (or the e2e tests) through `UDP_RELAY`.
    #[cfg(test)]
    pub(crate) async fn bind_for_test(addr: SocketAddr) -> Result<Arc<Self>> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Arc::new(UdpRelay {
            socket: Arc::new(socket),
            sessions: RwLock::new(HashMap::new()),
            counters: Counters::default(),
        }))
    }

    /// Bind + spawn the receive loop as a background task. Used by `main`.
    pub async fn start(addr: SocketAddr, stop: Trigger) -> Result<Arc<Self>> {
        let relay = Self::bind(addr).await?;
        let task_relay = relay.clone();
        tokio::spawn(async move { task_relay.run(stop).await });
        Ok(relay)
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    fn register(&self, session: &Arc<Session>) {
        let Some(udp) = session.udp() else {
            return;
        };
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = sessions.get(&udp.token)
            && prev.upgrade().is_some()
        {
            // 128-bit random tokens make this a measure-zero event; refuse
            // the collision rather than hijack another session's traffic.
            log::error!("UDP token collision on register; refusing new registration");
            return;
        }
        sessions.insert(udp.token, Arc::downgrade(session));
        log::debug!("Registered UDP token for session {:?}", session.id());
    }

    fn unregister(&self, token: &UdpToken) {
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        sessions.remove(token);
    }

    /// Main loop: receive datagrams from clients until `stop` fires.
    /// Also runs the periodic reaper and counter reporting.
    pub async fn run(self: Arc<Self>, stop: Trigger) {
        log::info!(
            "UDP relay listening on {}",
            self.local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "?".to_string())
        );
        let mut reaper = tokio::time::interval(std::time::Duration::from_secs(REAP_INTERVAL_SECS));
        let mut buf = vec![0u8; MAX_DATAGRAM_SIZE];
        loop {
            tokio::select! {
                _ = stop.wait_async() => {
                    log::info!("UDP relay stopping");
                    break;
                }
                _ = reaper.tick() => {
                    self.reap();
                    self.report_counters();
                }
                r = self.socket.recv_from(&mut buf) => {
                    match r {
                        Ok((len, src)) => self.handle_datagram(&buf[..len], src).await,
                        Err(e) => log::error!("UDP relay recv error: {:?}", e),
                    }
                }
            }
        }
    }

    async fn handle_datagram(self: &Arc<Self>, datagram: &[u8], src: SocketAddr) {
        let c = &self.counters;
        if datagram.len() < DATAGRAM_HEADER_SIZE {
            Counters::bump(&c.discarded_short);
            return;
        }
        Counters::bump(&c.received);

        let token: UdpToken = datagram[..TOKEN_LENGTH]
            .try_into()
            .expect("slice length checked above");

        let session = {
            let sessions = self.sessions.read().unwrap_or_else(|e| e.into_inner());
            sessions.get(&token).and_then(Weak::upgrade)
        };
        let Some(session) = session else {
            // Unknown token, or the session died: drop the stale entry.
            Counters::bump(&c.discarded_unknown);
            self.unregister(&token);
            return;
        };
        let Some(udp) = session.udp() else {
            // UDP leg was reaped; drop the stale token.
            Counters::bump(&c.discarded_unknown);
            self.unregister(&token);
            return;
        };

        let payload = {
            let mut inbound = udp.lock_inbound();
            match inbound.decrypt(&token, datagram) {
                Ok(Some(payload)) => payload,
                Ok(None) => {
                    // Duplicate or too old for the replay window: normal on UDP.
                    Counters::bump(&c.discarded_replay);
                    return;
                }
                Err(e) => {
                    Counters::bump(&c.auth_fail);
                    log::debug!("UDP datagram auth failure from {}: {:?}", src, e);
                    return;
                }
            }
        };

        // Authentic datagram: the source address is now trusted for the
        // return path. The *ip* must match the tunnel peer: UDP has no
        // handshake, so a client holding the key can put any source address
        // on the wire, and without this check the relay would aim the
        // remote's reply stream at a third party it never talked to
        // (reflection / amplification). Port changes (NAT rebinding) are
        // still accepted. A datagram from a foreign ip authenticates — the
        // key holds — but it is never made the return target.
        if src.ip() == session.src_ip().ip() {
            udp.set_client_addr(src);
        } else {
            Counters::bump(&c.discarded_foreign_source);
            log::debug!(
                "UDP datagram from {} does not match tunnel peer {}; return path not re-pointed",
                src,
                session.src_ip()
            );
        }
        udp.touch();

        // Lazily create the per-session socket towards the UDP remote and
        // its return task on the first authenticated datagram. Building the
        // leg resolves `remotes[0]`, and the relay's receive loop is shared
        // by every session: when it keeps failing, do NOT repeat the work (or
        // the error log) for every datagram.
        let remote_sock = match udp.remote_socket() {
            Some(sock) => sock,
            None => {
                if !udp.leg_retry_allowed() {
                    Counters::bump(&c.discarded_leg_backoff);
                    return;
                }
                match self.create_remote_leg(&session, &udp).await {
                    Ok(sock) => sock,
                    Err(e) => {
                        udp.note_leg_failure();
                        log::error!(
                            "Failed to create UDP remote leg for session {:?}: {:?}",
                            session.id(),
                            e
                        );
                        return;
                    }
                }
            }
        };

        match remote_sock.send(&payload).await {
            Ok(_) => {
                Counters::bump(&c.forwarded);
                // Client upload towards the remote: payload bytes, on a
                // successful forward (same rule as the TCP streams).
                session
                    .traffic()
                    .add_sent(payload.len().try_into().unwrap_or(u64::MAX));
            }
            Err(e) => log::error!("UDP relay send to remote failed: {:?}", e),
        }
    }

    /// Create the per-session connected socket towards `remotes[0]` (the
    /// RDP host, same endpoint as TCP channel 1) and spawn the task that
    /// pumps its replies back to the authenticated client address.
    async fn create_remote_leg(
        self: &Arc<Self>,
        session: &Arc<Session>,
        udp: &Arc<UdpState>,
    ) -> Result<Arc<UdpSocket>> {
        // Double-checked locking: another datagram may have raced us.
        if let Some(sock) = udp.remote_socket() {
            return Ok(sock);
        }
        let remote = session
            .remotes()
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session has no remotes"))?;
        let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        sock.connect(&remote).await.map_err(|e| {
            anyhow::anyhow!("failed to connect UDP socket to remote {}: {:?}", remote, e)
        })?;
        udp.set_remote_socket(sock.clone());

        self.spawn_return_task(session, udp.clone(), sock.clone());
        Ok(sock)
    }

    /// Return path: cleartext from the UDP remote -> encrypt -> client.
    /// Exits when the session stops or when the UDP leg is no longer the
    /// one installed on the session (reaper cleared it).
    fn spawn_return_task(
        self: &Arc<Self>,
        session: &Arc<Session>,
        udp: Arc<UdpState>,
        remote_sock: Arc<UdpSocket>,
    ) {
        let relay = self.clone();
        let session_weak = Arc::downgrade(session);
        let stop = session.stopper();
        let traffic = session.traffic();
        tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM_PAYLOAD];
            let mut alive_check =
                tokio::time::interval(std::time::Duration::from_secs(RETURN_ALIVE_CHECK_SECS));
            loop {
                tokio::select! {
                    _ = stop.wait_async() => break,
                    _ = alive_check.tick() => {
                        let gone = session_weak
                            .upgrade()
                            .and_then(|s| s.udp())
                            .is_none_or(|cur| !Arc::ptr_eq(&cur, &udp));
                        if gone {
                            break;
                        }
                    }
                    r = remote_sock.recv(&mut buf) => {
                        let len = match r {
                            Ok(len) => len,
                            Err(e) => {
                                log::error!("UDP remote recv error: {:?}", e);
                                break;
                            }
                        };
                        // Anti-amplification: never send to an address that
                        // has not produced a valid authenticated datagram.
                        let Some(client_addr) = udp.client_addr() else {
                            Counters::bump(&relay.counters.not_sent_no_addr);
                            continue;
                        };
                        let datagram = {
                            let mut outbound = udp.lock_outbound();
                            match outbound.encrypt(&udp.token, &buf[..len]) {
                                Ok(d) => d,
                                Err(e) => {
                                    log::error!("UDP outbound encrypt failed: {:?}", e);
                                    continue;
                                }
                            }
                        };
                        match relay.socket.send_to(&datagram, client_addr).await {
                            Ok(_) => {
                                Counters::bump(&relay.counters.sent);
                                // Download towards the client: the payload
                                // length, same rule as the upload above.
                                traffic.add_recv(len as u64);
                            }
                            Err(e) => log::error!("UDP relay send to client failed: {:?}", e),
                        }
                    }
                }
            }
        });
    }

    /// Drop tokens of dead sessions and tear down UDP legs idle for more
    /// than `UDP_IDLE_TIMEOUT_SECS` (the TCP session survives).
    fn reap(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut stale_tokens = Vec::new();
        {
            let sessions = self.sessions.read().unwrap_or_else(|e| e.into_inner());
            for (token, weak) in sessions.iter() {
                match weak.upgrade() {
                    None => stale_tokens.push(*token), // session died
                    Some(session) => match session.udp() {
                        None => stale_tokens.push(*token), // leg already cleared
                        Some(udp) => {
                            if now.saturating_sub(udp.last_activity()) > UDP_IDLE_TIMEOUT_SECS {
                                log::info!(
                                    "Reaping idle UDP leg of session {:?} (idle > {}s)",
                                    session.id(),
                                    UDP_IDLE_TIMEOUT_SECS
                                );
                                session.clear_udp();
                                stale_tokens.push(*token);
                            }
                        }
                    },
                }
            }
        }
        if !stale_tokens.is_empty() {
            let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
            for token in &stale_tokens {
                sessions.remove(token);
            }
        }
    }

    fn report_counters(&self) {
        let c = &self.counters;
        let received = c.received.load(Ordering::Relaxed);
        if received == 0 {
            return;
        }
        log::info!(
            "UDP relay stats: received={} forwarded={} sent={} short={} unknown_token={} replay={} auth_fail={} foreign_source={} leg_backoff={} no_addr_drops={}",
            received,
            c.forwarded.load(Ordering::Relaxed),
            c.sent.load(Ordering::Relaxed),
            c.discarded_short.load(Ordering::Relaxed),
            c.discarded_unknown.load(Ordering::Relaxed),
            c.discarded_replay.load(Ordering::Relaxed),
            c.auth_fail.load(Ordering::Relaxed),
            c.discarded_foreign_source.load(Ordering::Relaxed),
            c.discarded_leg_backoff.load(Ordering::Relaxed),
            c.not_sent_no_addr.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod audit;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_hostile;

#[cfg(test)]
mod tests_relay_invariants;
