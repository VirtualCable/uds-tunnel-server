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
    //
    // Convention: `seq.0`/`seq.1` start at `(0, 0)` and are **not** a
    // reflection of crypt state. The session is constructed in `new()`
    // without crypts; whoever builds the first pair of crypts (only
    // `connection::connect` in production) does so via
    // `server_tunnel_crypts()`, which reads these values, then issues
    // the handshake that consumes one seq each direction. After the
    // handshake completes, `connect` calls `set_seqs(1, 1)` to bring
    // the session's seqs in line with what the crypts actually
    // consumed.
    //
    // Any code reading `session.seqs()` (or calling `fetch_add_seqs`)
    // BEFORE that explicit sync must assume `(0, 0)` is *by design* —
    // not a missing update. This is fine because the only producer is
    // `connect`, which is the sole owner of the just-created session
    // for the brief window between `Session::new` and the handshake.
    // (Recover uses an existing session whose seqs are already past
    // the initial handshake.)
    //
    // **Why `(0, 0)` and not `(1, 1)`**: this is the initial value the
    // tunnel client (udstunnel in `openuds/client`) expects when it
    // builds its first pair of crypts for the handshake. Both sides
    // MUST start at the same number, otherwise the crypt anti-replay
    // check (`seq < current_seq` in `crypt::Crypt::decrypt`) rejects
    // the very first encrypted packet and the handshake fails. The
    // contract is "both sides, `(0, 0)`"; bumping it to `(1, 1)` here
    // without coordinating with the client would silently break every
    // inbound connection. The integration tests in
    // `connection/tests.rs::create_out_int_crypts` pin this contract
    // (they build the client-side crypt with `Crypt::new(&key, 0)`),
    // so any change here must update them in lockstep.
    seq: RwLock<(u64, u64)>,

    // External (equiv) session id the client uses to talk to us. `None`
    // until the first Recover mints one, after which it is the only
    // valid id for this session (the internal `id` is never exposed).
    current_equiv_id: RwLock<Option<SessionId>>,

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
            current_equiv_id: RwLock::new(None),
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

    /// Returns the current `src_ip` recorded for this session.
    /// Cheap (single read-lock); used by `SessionManager::count_by_remote`.
    pub fn src_ip(&self) -> SocketAddr {
        // Ignore the poison: if the lock was poisoned by a previous
        // panic we still want the inner value back so the caller can
        // continue to operate on the session.
        *self.src_ip.read().unwrap_or_else(|e| e.into_inner())
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

    /// Atomically assign the (inbound, outbound) sequence numbers in a
    /// single critical section. Use this whenever a session ends up
    /// with both seqs known at the same time (initial handshake, server
    /// stream teardown, recover), so the two halves of the pair cannot
    /// drift apart under a concurrent observer.
    ///
    /// When only the relative advance is known (e.g. "advance both by 1
    /// to allocate the next seq pair"), prefer [`Self::fetch_add_seqs`]
    /// which returns the pre-increment values for an `OpenResponse`.
    pub fn set_seqs(&self, seq_rx: u64, seq_tx: u64) {
        let mut seq_lock = self.seq.write().unwrap_or_else(|e| e.into_inner());
        seq_lock.0 = seq_rx;
        seq_lock.1 = seq_tx;
    }

    // Returns the (inbound, outbound) seq numbers
    //
    // A poisoned lock must not be reported as `(0, 0)`: that is the
    // legitimate pre-handshake value, so the caller cannot tell a real
    // seq pair from a failed read, and `server_tunnel_crypts()` would
    // silently build the crypts at seq 0 and trip the anti-replay check.
    pub fn seqs(&self) -> (u64, u64) {
        *self.seq.read().unwrap_or_else(|e| e.into_inner())
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
        let mut seq_lock = self.seq.write().unwrap_or_else(|e| e.into_inner());
        let prev = *seq_lock;
        seq_lock.0 = prev.0.wrapping_add(in_delta);
        seq_lock.1 = prev.1.wrapping_add(out_delta);
        prev
    }

    /// Set or clear the external (equiv) session id that the client uses
    /// to talk to this session. There is at most one valid equiv id at
    /// any time; setting a new one implicitly invalidates the previous
    /// because nothing else stores the old value.
    pub fn set_current_equiv_id(&self, id: Option<SessionId>) {
        if let Ok(mut lock) = self.current_equiv_id.write() {
            *lock = id;
        }
    }

    /// Returns the current external (equiv) session id, or `None` if the
    /// session has not yet been addressed by a Recover handshake.
    pub fn current_equiv_id(&self) -> Option<SessionId> {
        self.current_equiv_id.read().ok().and_then(|g| *g)
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

    /// Regression guard for the split-setter pair that `set_seqs`
    /// replaced: assigning the two halves in separate critical sections
    /// lets a concurrent observer read a **torn** pair (new inbound,
    /// stale outbound). Both phases write symmetric pairs, so any
    /// observed `(a, b)` with `a != b` is a torn read.
    ///
    /// The split phase is kept deliberately: it is what makes the
    /// assertion on `set_seqs` meaningful instead of vacuous, and it
    /// fails loudly if anyone splits the assignment again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn set_seqs_is_never_observed_torn() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // The pre-`b0d8e37` implementation, kept only as the control
        // group: set_inbound_seq followed by set_outbound_seq, each
        // taking the lock on its own.
        fn set_split(session: &Session, seq_rx: u64, seq_tx: u64) {
            session.seq.write().unwrap_or_else(|e| e.into_inner()).0 = seq_rx;
            session.seq.write().unwrap_or_else(|e| e.into_inner()).1 = seq_tx;
        }

        async fn count_torn_reads(split: bool) -> usize {
            const READS: usize = 2_000_000;

            let session = Arc::new(new_test_session().await);
            let stop = Arc::new(AtomicBool::new(false));

            let writer = {
                let session = session.clone();
                let stop = stop.clone();
                tokio::task::spawn_blocking(move || {
                    let mut n = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        if split {
                            set_split(&session, n, n);
                        } else {
                            session.set_seqs(n, n);
                        }
                        n = n.wrapping_add(1);
                    }
                })
            };

            let reader = {
                let session = session.clone();
                tokio::task::spawn_blocking(move || {
                    (0..READS)
                        .filter(|_| {
                            let (inbound, outbound) = session.seqs();
                            inbound != outbound
                        })
                        .count()
                })
            };

            let torn = reader.await.expect("reader panicked");
            stop.store(true, Ordering::Relaxed);
            writer.await.expect("writer panicked");
            torn
        }

        let torn_split = count_torn_reads(true).await;
        let torn_atomic = count_torn_reads(false).await;

        log::debug!("torn reads -- split: {torn_split}, set_seqs: {torn_atomic}");

        assert!(
            torn_split > 0,
            "control group observed no torn read, so the set_seqs assertion proves nothing"
        );
        assert_eq!(
            torn_atomic, 0,
            "set_seqs exposed a half-updated pair {torn_atomic} time(s)"
        );
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

    /// Concurrent fetches must each observe a unique pre-increment pair.
    /// This is the regression test for the TOCTOU race in the Recover
    /// handler: before the atomic helper was added, two parallel recovers
    /// on the same session could both read the same pre-increment value
    /// and overwrite each other's `set_*_seq` updates.
    #[serial_test::serial(manager)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fetch_add_seqs_is_atomic_under_concurrency() {
        use crate::session::SessionManager;

        let session = new_test_session().await;
        let session = SessionManager::get_instance()
            .add_session(session)
            .expect("session id collision unlikely with random ticket");
        let session: Arc<Session> = session;

        const N: u64 = 500;
        let mut handles = Vec::new();
        for _ in 0..4 {
            let s = session.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                let mut observed = Vec::with_capacity(N as usize);
                for _ in 0..N {
                    observed.push(s.fetch_add_seqs(1, 1));
                }
                observed
            }));
        }

        let mut all: Vec<(u64, u64)> = futures::future::join_all(handles)
            .await
            .into_iter()
            .flat_map(|h| h.expect("task panicked"))
            .collect();

        // Every observation must be unique — no two callers saw the same
        // (inbound, outbound) pair, which is what the old TOCTOU code
        // would have produced.
        all.sort();
        let original_len = all.len();
        all.dedup();
        assert_eq!(
            all.len(),
            original_len,
            "fetch_add_seqs returned duplicate pairs (TOCTOU race)"
        );

        // The observed pre-increment pairs must form a contiguous range
        // [0, N*4) with no gaps. A gap would indicate that some caller
        // saw a duplicate pre-increment and skipped a value.
        let min = *all.first().expect("at least one observation");
        let max = *all.last().expect("at least one observation");
        assert_eq!(min, (0, 0));
        assert_eq!(max, (N * 4 - 1, N * 4 - 1));

        // And the final seqs must equal the number of increments.
        let (final_in, final_out) = session.seqs();
        assert_eq!(final_in, N * 4);
        assert_eq!(final_out, N * 4);
    }

    /// `current_equiv_id` starts as `None` on a fresh session, accepts
    /// arbitrary `Some(_)` writes, and accepts a clear back to `None`.
    /// This is the atomicity guarantee of the unit backing
    /// `SessionManager::get_equiv_session` / `create_equiv_session`.
    #[tokio::test]
    async fn current_equiv_id_starts_none_and_round_trips() {
        let session = new_test_session().await;
        assert!(
            session.current_equiv_id().is_none(),
            "fresh session must not carry an equiv id"
        );

        let id = ticket::Ticket::new_random();
        session.set_current_equiv_id(Some(id));
        assert_eq!(session.current_equiv_id(), Some(id));

        session.set_current_equiv_id(None);
        assert!(
            session.current_equiv_id().is_none(),
            "clearing the equiv id must restore the initial state"
        );
    }

    /// Writing a new equiv id overwrites the previous one without
    /// leaving the old value around. This is the property that lets
    /// phase 2 drop the old `HashMap<SessionId, SessionId>`: the
    /// session itself owns the slot, so a new write is implicitly a
    /// drop of the previous one.
    #[tokio::test]
    async fn set_current_equiv_id_overwrites_previous_value() {
        let session = new_test_session().await;
        let first = ticket::Ticket::new_random();
        let second = ticket::Ticket::new_random();

        session.set_current_equiv_id(Some(first));
        assert_eq!(session.current_equiv_id(), Some(first));

        // Same pattern the recover flow uses: a fresh equiv id replaces
        // the old one without an explicit clear in between.
        session.set_current_equiv_id(Some(second));
        assert_eq!(
            session.current_equiv_id(),
            Some(second),
            "second write must overwrite the first, not stack on top"
        );
        assert_ne!(
            session.current_equiv_id(),
            Some(first),
            "old equiv id must not leak through after overwrite"
        );
    }
}
