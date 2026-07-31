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

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize},
    },
};

use anyhow::Result;

use shared::{
    crypt::{self, types::SharedSecret},
    log,
    protocol::{
        PayloadWithChannelReceiver, PayloadWithChannelSender, payload_with_channel_pair, ticket,
    },
    system::trigger::Trigger,
};

mod buffer;
mod manager;
mod proxy;

pub use {
    buffer::{BufferedPacket, RecoveryError, RecoverySendBuffer},
    manager::SessionManager,
    proxy::types::{ClientEndpoints, ServerEndpoints},
};

// Alias, internal SessionId is a Ticket
pub type SessionId = ticket::Ticket;

pub static RECOVERY_BUFFER_SIZE: AtomicUsize = AtomicUsize::new(64 * 1024); // Default to 64 KB, can be configured at runtime

#[derive(Debug, Clone)]
pub struct SessionRecoveryBuffer(Arc<Mutex<RecoverySendBuffer>>);

// Arc<Mutex<T>> is automatically Send + Sync when T: Send, so no manual
// unsafe impl is needed. The previous Rc<UnsafeCell<T>> design with hand-
// rolled unsafe impl Send/Sync was unsound: get() returned a &mut through
// a shared reference, which is undefined behaviour if two tasks (e.g. an
// in-flight server outbound push and a concurrent Recover handler
// calling skip on the same session) ever observed the cell at once.

impl SessionRecoveryBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self(Arc::new(Mutex::new(RecoverySendBuffer::new(max_bytes))))
    }

    /// Lock the underlying buffer for exclusive access. The critical
    /// section is short (push/pop/skip are O(1) or O(n) over a small n),
    /// so a std::sync::Mutex is appropriate; the lock is released as
    /// soon as the returned guard goes out of scope.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, RecoverySendBuffer> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug)]
pub struct Session {
    id: SessionId,
    ticket: ticket::Ticket,
    shared_secret: SharedSecret,
    stop: Trigger,
    // Channels for server <-> client communication
    session_proxy: proxy::handler::Handler,

    // proxy async task handle
    proxy_task: tokio::task::JoinHandle<()>,
    // Server side status
    server_running: AtomicBool,
    // If the server side has error on exit
    close_notified: AtomicBool,

    // Session is closed when:
    //   - client (connetecto to ou server side) disconnects correctly
    //   - client sends a Close command
    //   - client does not reconnect on recovery window
    remotes: Vec<String>, // List of remote addresses that can be used on this session

    // If there is an unsent message on server side
    // (eg: client sent a message but an error ocurrend, and it's alreade consumed from channel)
    recovery_buffer: SessionRecoveryBuffer,

    // The channels for server side must be kept in the session, as they can contain unprocessed messages
    tx: PayloadWithChannelSender,
    rx_server: PayloadWithChannelReceiver,
    tx_server: PayloadWithChannelSender,
    rx: PayloadWithChannelReceiver,

    // seq numbers for crypto part
    // only updated on server side killed. (the one receives/sends data from client)
    seq: RwLock<(u64, u64)>,

    // Ip of the client connected
    src_ip: RwLock<SocketAddr>,
}

impl Session {
    pub fn new(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stop: Trigger,
        src_ip: SocketAddr,
        remotes: Vec<String>, // List of remote addresses that can be used on this session
    ) -> Self {
        let (proxy, session_proxy) = proxy::Proxy::new(stop.clone());
        let id = SessionId::new_random();

        let proxy_task = proxy.run(id); // Start proxy task

        let (tx, rx_server) = payload_with_channel_pair();
        let (tx_server, rx) = payload_with_channel_pair();

        Session {
            id,
            ticket,
            shared_secret,
            stop,
            session_proxy,
            proxy_task,
            server_running: AtomicBool::new(false),
            close_notified: AtomicBool::new(false),
            recovery_buffer: SessionRecoveryBuffer::new(
                RECOVERY_BUFFER_SIZE.load(std::sync::atomic::Ordering::Relaxed),
            ),
            tx,
            rx_server,
            tx_server,
            rx,
            seq: RwLock::new((0, 0)),
            src_ip: RwLock::new(src_ip),
            remotes,
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn recovery_buffer(&self) -> SessionRecoveryBuffer {
        self.recovery_buffer.clone()
    }

    pub fn is_close_notified(&self) -> bool {
        self.close_notified
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn close_notified(&self) {
        self.close_notified
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Note: Even cloned, ther will be only one server side per session, so this is all fine.
    pub fn get_server_channels(&self) -> (PayloadWithChannelSender, PayloadWithChannelReceiver) {
        (self.tx_server.clone(), self.rx_server.clone())
    }

    pub fn get_proxy_channels(&self) -> (PayloadWithChannelSender, PayloadWithChannelReceiver) {
        (self.tx.clone(), self.rx.clone())
    }

    pub fn set_ip(&self, ip: SocketAddr) {
        if let Ok(mut ip_lock) = self.src_ip.write() {
            *ip_lock = ip;
        }
    }

    pub async fn start_server(&self) -> Result<ServerEndpoints> {
        self.server_running
            .store(true, std::sync::atomic::Ordering::Relaxed);

        self.session_proxy.start_server().await
    }

    pub(super) async fn stop_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.stop_server().await;
    }

    pub fn set_inbound_seq(&self, seq_rx: u64) {
        if let Ok(mut seq_lock) = self.seq.write() {
            seq_lock.0 = seq_rx;
        }
    }

    pub fn set_outbound_seq(&self, seq_tx: u64) {
        if let Ok(mut seq_lock) = self.seq.write() {
            seq_lock.1 = seq_tx;
        }
    }

    // Returns the (inbound, outbound) seq numbers
    pub fn seqs(&self) -> (u64, u64) {
        if let Ok(seq_lock) = self.seq.read() {
            *seq_lock
        } else {
            (0, 0)
        }
    }

    /// Atomically read the current (inbound, outbound) sequence numbers
    /// and add the given deltas in a single critical section. Returns the
    /// **pre-increment** values so the caller can attach them to an
    /// `OpenResponse` and the next expected seq is `prev + delta`.
    ///
    /// Use this instead of `seqs()` + `set_*_seq()` whenever the read and
    /// the write must not be split across concurrent callers (for example
    /// the Recover handler, which used to TOCTOU-race itself when two
    /// recovery attempts on the same session interleaved).
    pub fn fetch_add_seqs(&self, in_delta: u64, out_delta: u64) -> (u64, u64) {
        let mut seq_lock = self
            .seq
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let prev = *seq_lock;
        seq_lock.0 = prev.0.wrapping_add(in_delta);
        seq_lock.1 = prev.1.wrapping_add(out_delta);
        prev
    }

    pub fn ticket(&self) -> &ticket::Ticket {
        &self.ticket
    }

    pub fn shared_secret(&self) -> &SharedSecret {
        &self.shared_secret
    }

    pub fn stopper(&self) -> Trigger {
        self.stop.clone()
    }

    pub fn is_running(&self) -> bool {
        !self.proxy_task.is_finished()
    }

    pub fn is_server_running(&self) -> bool {
        self.server_running
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        crypt::tunnel::get_tunnel_crypts(&self.shared_secret, self.ticket(), self.seqs())
    }

    pub(super) async fn fail_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.fail_server().await;
    }

    pub(super) async fn stop_client(&self, stream_channel_id: u16) {
        self.session_proxy.stop_client(stream_channel_id).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.trigger();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that creates a fresh session with the test plumbing ready
    /// (the proxy task spawned by `Session::new` is shut down on Drop).
    async fn new_test_session() -> Session {
        Session::new(
            SharedSecret::new([0u8; 32]),
            ticket::Ticket::new_random(),
            Trigger::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
        )
    }

    /// `fetch_add_seqs` must return the **pre-increment** values, so the
    /// caller can attach them straight to an `OpenResponse` and the seqs
    /// stored in the session already reflect `prev + delta`. This is the
    /// exact pattern used by `connection::recover::recover`.
    #[tokio::test]
    async fn fetch_add_seqs_returns_pre_increment_and_advances_in_one_step() {
        let session = new_test_session().await;

        // First recover handshake: seqs start at (0, 0). The caller will
        // build an OpenResponse carrying (0, 0) and the session must end
        // up at (1, 1) afterwards.
        let (in_seq, out_seq) = session.fetch_add_seqs(1, 1);
        assert_eq!((in_seq, out_seq), (0, 0));
        assert_eq!(session.seqs(), (1, 1));

        // Second recover on the same session: must observe (1, 1) as the
        // pre-increment and leave the session at (2, 2). This is the
        // property that the original TOCTOU pattern failed to provide.
        let (in_seq, out_seq) = session.fetch_add_seqs(1, 1);
        assert_eq!((in_seq, out_seq), (1, 1));
        assert_eq!(session.seqs(), (2, 2));
    }

    /// Zero-delta fetch is a pure read: returns current and does not move
    /// the counters.
    #[tokio::test]
    async fn fetch_add_seqs_with_zero_delta_is_pure_read() {
        let session = new_test_session().await;
        session.fetch_add_seqs(7, 11);

        let (in_seq, out_seq) = session.fetch_add_seqs(0, 0);
        assert_eq!((in_seq, out_seq), (7, 11));
        assert_eq!(session.seqs(), (7, 11));
    }

    /// Asymmetric deltas work: the caller can advance only one side if
    /// needed (recover always uses 1, 1 but the helper should be general).
    #[tokio::test]
    async fn fetch_add_seqs_supports_asymmetric_deltas() {
        let session = new_test_session().await;
        let (in_seq, out_seq) = session.fetch_add_seqs(3, 0);
        assert_eq!((in_seq, out_seq), (0, 0));
        assert_eq!(session.seqs(), (3, 0));

        let (in_seq, out_seq) = session.fetch_add_seqs(0, 5);
        assert_eq!((in_seq, out_seq), (3, 0));
        assert_eq!(session.seqs(), (3, 5));
    }
}
