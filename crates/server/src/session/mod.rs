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
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;

use shared::{
    crypt::{
        self,
        datagram::{DatagramCrypt, UdpToken},
        rekey::{MAX_REKEY_LOG2, SessionPrk},
        types::SharedSecret,
    },
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

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Per-session state for the UDP relay leg. Owned by the `Session`, dies
/// with it (or earlier, if the relay's inactivity reaper clears it; the
/// TCP session survives that).
///
/// Lock poisoning is recovered with `unwrap_or_else(|e| e.into_inner())`
/// everywhere, mirroring the session seq lock: the critical sections are
/// short and non-recursive, so a poisoned lock still holds a usable value.
pub struct UdpState {
    pub token: UdpToken,
    pub inbound: Mutex<DatagramCrypt>, // client -> server (decrypt)
    pub outbound: Mutex<DatagramCrypt>, // server -> client (encrypt)
    pub client_addr: RwLock<Option<SocketAddr>>,
    pub last_activity: AtomicU64, // secs since unix epoch
    // Per-session socket towards the UDP remote (the RDP host), created
    // on demand by the relay on the first authenticated datagram.
    remote_socket: RwLock<Option<Arc<tokio::net::UdpSocket>>>,
    // Unix secs before which a *fresh* remote-leg creation attempt is skipped
    // after a failure. Creating the leg resolves `remotes[0]`; repeating that
    // inline on the shared relay loop for every datagram of a session whose
    // remote does not resolve would stall every other session (and emit one
    // error log per datagram).
    leg_retry_not_before: AtomicU64,
}

// DatagramCrypt has no Debug impl; show the useful bits instead.
// The relay token is a credential: redact its middle so it never lands
// in logs in full.
impl std::fmt::Debug for UdpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdpState")
            .field("token", &shared::log::redact_secret_bytes(&self.token))
            .field("client_addr", &self.client_addr())
            .field("last_activity", &self.last_activity())
            .finish()
    }
}

impl UdpState {
    pub fn new(token: UdpToken, inbound: DatagramCrypt, outbound: DatagramCrypt) -> Self {
        UdpState {
            token,
            inbound: Mutex::new(inbound),
            outbound: Mutex::new(outbound),
            client_addr: RwLock::new(None),
            last_activity: AtomicU64::new(unix_now_secs()),
            remote_socket: RwLock::new(None),
            leg_retry_not_before: AtomicU64::new(0),
        }
    }

    pub fn lock_inbound(&self) -> std::sync::MutexGuard<'_, DatagramCrypt> {
        self.inbound.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn lock_outbound(&self) -> std::sync::MutexGuard<'_, DatagramCrypt> {
        self.outbound.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Address of the authenticated UDP peer, if any datagram has
    /// already decrypted correctly (anti-amplification gate).
    pub fn client_addr(&self) -> Option<SocketAddr> {
        *self.client_addr.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Update the client address after a successful decrypt. Safe for
    /// NAT rebinding: only authenticated datagrams reach this point.
    pub fn set_client_addr(&self, addr: SocketAddr) {
        let mut lock = self.client_addr.write().unwrap_or_else(|e| e.into_inner());
        if *lock != Some(addr) {
            *lock = Some(addr);
        }
    }

    pub fn touch(&self) {
        self.last_activity
            .store(unix_now_secs(), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn last_activity(&self) -> u64 {
        self.last_activity
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn remote_socket(&self) -> Option<Arc<tokio::net::UdpSocket>> {
        self.remote_socket
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_remote_socket(&self, socket: Arc<tokio::net::UdpSocket>) {
        *self
            .remote_socket
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(socket);
    }

    /// Cooldown between remote-leg creation attempts after a failure. Bounds
    /// the per-session work (and log volume) a session whose remote cannot be
    /// built can impose on the shared UDP relay loop.
    const LEG_RETRY_BACKOFF_SECS: u64 = 30;

    /// Record a failed remote-leg creation and arm the cooldown, so the next
    /// datagram for this session does not repeat the (possibly slow) address
    /// resolution inline on the shared relay loop.
    pub fn note_leg_failure(&self) {
        self.leg_retry_not_before.store(
            unix_now_secs().saturating_add(Self::LEG_RETRY_BACKOFF_SECS),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// True when a fresh remote-leg creation attempt may be made.
    pub fn leg_retry_allowed(&self) -> bool {
        unix_now_secs()
            >= self
                .leg_retry_not_before
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test hook: the armed cooldown instant (0 when no failure was recorded).
    #[cfg(test)]
    pub(crate) fn leg_retry_not_before_for_test(&self) -> u64 {
        self.leg_retry_not_before
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub static RECOVERY_BUFFER_SIZE: AtomicUsize = AtomicUsize::new(64 * 1024); // Default to 64 KB, can be configured at runtime

/// Handle identifying the launcher-facing server stream that currently
/// owns the session's server side.
///
/// One is created per attach (`Session::start_server`) and stored in the
/// session slot. Its identity is the `Arc` itself: a stream may only run
/// the proxy teardown path while it is still the slot owner (`Arc::ptr_eq`
/// through `Session::is_current_server_stream`), so a stream replaced by a
/// recovery cannot `fail_server`/`stop_server` the attachment that took
/// over (the proxy keeps a single server-side channel set).
///
/// `stop` is fired when a newer stream replaces this one, so a stale
/// stream cannot keep pumping frames into the proxy or hold its socket
/// read half forever.
///
/// Sequence numbers need no coordination between streams: the session's
/// per-direction counters are shared with every crypt the session hands
/// out, so an outgoing stream's last in-flight frame and the new stream's
/// first frame can never collide on a `(key, seq)` AES-GCM nonce.
#[derive(Debug)]
pub struct ServerStreamOwner {
    stop: Trigger,
}

impl ServerStreamOwner {
    fn new() -> Self {
        Self {
            stop: Trigger::new(),
        }
    }

    /// Stop trigger the attached stream must use for its tasks. Firing it
    /// (through `Session::start_server` replacing this owner) kills the
    /// stream immediately.
    pub fn stopper(&self) -> Trigger {
        self.stop.clone()
    }
}

/// Bytes relayed through a session, counted at the launcher-facing legs
/// of the tunnel: `sent` is client upload (launcher -> remote), `recv` is
/// download (remote -> launcher). Payload bytes (post-decrypt /
/// pre-encrypt), not wire bytes, and control-channel traffic is excluded.
/// Reported to the broker on session stop.
#[derive(Debug, Default)]
pub struct TrafficCounters {
    sent: AtomicU64,
    recv: AtomicU64,
}

impl TrafficCounters {
    pub fn add_sent(&self, bytes: u64) {
        self.sent
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn add_recv(&self, bytes: u64) {
        self.recv
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.sent.load(std::sync::atomic::Ordering::Relaxed),
            self.recv.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Total payload bytes counted so far (sent + recv). The sum is
    /// monotonically increasing and only moves when real tunnel data
    /// flows — channel-0 control frames (keep-alive `Nop`) never reach
    /// the counters by construction. The session data-idle watchdog
    /// rides on this: a zero delta across a full window means "alive
    /// leg, no data", which is exactly what the cap must expire.
    pub fn total(&self) -> u64 {
        self.sent
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(self.recv.load(std::sync::atomic::Ordering::Relaxed))
    }
}

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
    // Attach coordination for the launcher-facing server stream. Exactly one
    // such stream may be live per session:
    //   - `server_stream_slot` holds the attached stream's owner record. The
    //     record's identity is the `Arc` itself: ownership is decided with
    //     `Arc::ptr_eq`, so no counter/generation is needed. A replacement
    //     (`start_server`) kills the previous owner instantly: it takes the
    //     slot and fires the owner stop trigger under the slot write lock,
    //     then installs the new record. The dying stream's teardown must not
    //     touch the proxy state of its replacement, which the slot owner
    //     check (under the attach lock) guarantees.
    //   - `server_attach_lock` serializes [kill -> attach] with the dying
    //     stream's proxy teardown path, so two concurrent Recover handshakes
    //     cannot interleave and the replacement always happens while no
    //     teardown half-runs.
    server_stream_slot: RwLock<Option<Arc<ServerStreamOwner>>>,
    server_attach_lock: tokio::sync::Mutex<()>,
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

    // Live per-direction sequence counters for the launcher leg crypts.
    //
    // These are the *authoritative* counters: every `Crypt` the session
    // hands out (the `connect` handshake, each launcher-facing stream, each
    // recovery handshake) shares them via `Crypt::with_counter`, instead of
    // being seeded from a snapshot. The counter only ever moves when a frame
    // is actually encrypted (fetch_add, pre-increment) or authenticated
    // (fetch_max on decrypt), so no two crypts can use the same `(key, seq)`
    // AES-GCM nonce pair — not even while a killed/replaced stream and its
    // replacement overlap for the few frames still in flight.
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
    seq_in: Arc<AtomicU64>,
    seq_out: Arc<AtomicU64>,

    // Session-wide rekeying parameters, owned at handshake time and NEVER
    // renegotiated (a `Recover` reuses these exactly; docs/plan/rekeying.md
    // §3). `rekey_log2 = 0` is OFF (single key for the session lifetime,
    // the pre-rekeying wire format). The PRK lives here so the HKDF extract
    // runs once per session; every crypt pair rebuilds its `RekeyState`
    // from it (cheap, no secret cloning per stream).
    rekey_log2: u8,
    rekey_prk: Arc<SessionPrk>,

    // External (equiv) session id the client uses to talk to us. `None`
    // until the first Recover mints one, after which it is the only
    // valid id for this session (the internal `id` is never exposed).
    current_equiv_id: RwLock<Option<SessionId>>,

    // Ip of the client connected
    src_ip: RwLock<SocketAddr>,

    // UDP relay leg state, `Some` only when both the broker flag and the
    // server config allowed it at connect time. Cleared by the relay's
    // inactivity reaper; the TCP session survives that.
    udp: RwLock<Option<Arc<UdpState>>>,

    // Stop-notification ticket issued by the broker at `start_connection`
    // time. When present, the session tells the broker the tunnel is over
    // (and reports traffic stats) exactly once, on teardown. `None` for
    // sessions built without a broker round-trip (tests, recover-only
    // plumbing) or when the broker's notify ticket is malformed.
    broker_stop_ticket: Option<ticket::Ticket>,

    /// Bytes relayed through this session, for the stop report. Shared
    /// with every launcher-facing stream/relay task so counts accumulate
    /// across reconnects (a recover reuses the same session).
    traffic: Arc<TrafficCounters>,

    /// Claims the one-shot broker stop notification. `Session::Drop` is
    /// the only teardown choke-point (close, cap-reject, proxy exit,
    /// shutdown all funnel here), and the flag makes it idempotent.
    broker_notified: AtomicBool,
}

impl Session {
    pub fn new(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stop: Trigger,
        src_ip: SocketAddr,
        remotes: Vec<String>, // List of remote addresses that can be used on this session
    ) -> Self {
        Self::with_broker_stop_ticket(shared_secret, ticket, stop, src_ip, remotes, None)
    }

    /// Rekey threshold (`k`) pinned to this session at handshake time.
    /// `0` = OFF. This is the single source of truth for the
    /// `OpenResponse.rekey_log2` the server advertises and for every crypt
    /// the session hands out (streams, replacements, recovery): none of
    /// them re-reads the config, so a Recover can never drift the key
    /// epoching of an ongoing session.
    pub fn rekey_log2(&self) -> u8 {
        self.rekey_log2
    }

    /// Build a session that must notify the broker when it ends.
    ///
    /// `broker_stop_ticket` is the notify ticket the broker returned at
    /// `start_connection` time; on teardown the session sends the stop
    /// command with it (once) so the broker closes the tunnel record and
    /// receives the traffic stats. Pass `None` (or use `new`) for
    /// sessions that never went through a broker start.
    pub fn with_broker_stop_ticket(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stop: Trigger,
        src_ip: SocketAddr,
        remotes: Vec<String>,
        broker_stop_ticket: Option<ticket::Ticket>,
    ) -> Self {
        // OFF (k = 0): the legacy single-key construction. Sessions that
        // must rekey are built through `with_rekey_log2`; the production
        // handshake (connection::connect) always goes through it, so this
        // wrapper is only the tests'/manual-plumbing default.
        Self::with_rekey_log2(
            shared_secret,
            ticket,
            stop,
            src_ip,
            remotes,
            broker_stop_ticket,
            0,
        )
    }

    /// Build a session with the rekeying threshold `k` (log2 of frames per
    /// AES-GCM key epoch; `0` = OFF) adopted at the `Open` handshake.
    /// `k` values above the shift-safe bound (`MAX_REKEY_LOG2`) would make
    /// `seq >> k` undefined, so they are clamped (the config getter
    /// normally already clamped them).
    pub fn with_rekey_log2(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stop: Trigger,
        src_ip: SocketAddr,
        remotes: Vec<String>,
        broker_stop_ticket: Option<ticket::Ticket>,
        k: u8,
    ) -> Self {
        let k = k.min(MAX_REKEY_LOG2);
        // Derive the session PRK before the material is moved into the
        // struct: one HKDF extract per session, shared by every crypt pair
        // the session hands out afterwards.
        let rekey_prk = Arc::new(SessionPrk::derive(&shared_secret, &ticket));

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
            seq_in: Arc::new(AtomicU64::new(0)),
            seq_out: Arc::new(AtomicU64::new(0)),
            rekey_log2: k,
            rekey_prk,
            current_equiv_id: RwLock::new(None),
            src_ip: RwLock::new(src_ip),
            remotes,
            udp: RwLock::new(None),
            server_stream_slot: RwLock::new(None),
            server_attach_lock: tokio::sync::Mutex::new(()),
            broker_stop_ticket,
            traffic: Arc::new(TrafficCounters::default()),
            broker_notified: AtomicBool::new(false),
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

    pub async fn start_server(&self) -> Result<(ServerEndpoints, Arc<ServerStreamOwner>)> {
        // A fresh launcher connection replaces any still-live server stream
        // before taking over the proxy channel set: kill the previous owner
        // instantly (no drain wait) and swap the slot under the write lock,
        // so a dying stream's teardown can never observe itself as current
        // after the replacement attached. Sequence safety does not need a
        // drain: the replacement's crypts share the session counters, so the
        // replaced stream's last in-flight frames and the new stream's first
        // ones use disjoint `(key, seq)` nonce pairs by construction.
        // (In practice a second launcher cannot attach while the first holds
        // the connection, so this is free on the normal first-attach path
        // where the slot is empty.)
        self.kill_current_server_stream();
        self.server_running
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let endpoints = match self.session_proxy.start_server().await {
            Ok(endpoints) => endpoints,
            Err(e) => {
                // The proxy refused or died while attaching: do not leave the
                // session claiming a server stream that never attached.
                self.server_running
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return Err(e);
            }
        };
        let owner = Arc::new(ServerStreamOwner::new());
        *self
            .server_stream_slot
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(owner.clone());
        Ok((endpoints, owner))
    }

    /// Instantly kills the current server stream (if any): takes it out of
    /// the attach slot and fires its stop trigger, so its pump tasks unwind
    /// without ever being able to run the proxy teardown path (the slot no
    /// longer points at it). No drain/publish wait: with the crypts sharing
    /// the session counters, a killed stream's remaining in-flight frames
    /// cannot collide with the replacement stream's sequence numbers.
    pub(crate) fn kill_current_server_stream(&self) {
        let previous = self
            .server_stream_slot
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(previous) = previous {
            previous.stopper().trigger();
        }
    }

    /// Serialize the attach critical section ([kill previous owner ->
    /// allocate proxy channel set -> swap slot]) with the dying stream's
    /// proxy teardown path. Two concurrent Recover handshakes on the same
    /// session cannot interleave either.
    pub(crate) async fn lock_server_attach(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.server_attach_lock.lock().await
    }

    /// True when `owner` is the stream currently registered in the attach
    /// slot. A stream that died naturally may still have been replaced by a
    /// recovery between its last check and its teardown; ownership is only
    /// meaningful under the attach lock, which the replacement's `start_server`
    /// also runs under.
    pub(crate) fn is_current_server_stream(&self, owner: &Arc<ServerStreamOwner>) -> bool {
        let slot = self
            .server_stream_slot
            .read()
            .unwrap_or_else(|e| e.into_inner());
        slot.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, owner))
    }

    pub(super) async fn stop_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.stop_server().await;
    }

    /// Returns the (inbound, outbound) seq numbers.
    ///
    /// These are the live shared counters: the values are whatever the
    /// session's crypts last consumed/advanced, with no separate "published"
    /// state to keep in sync.
    pub fn seqs(&self) -> (u64, u64) {
        (
            self.seq_in.load(std::sync::atomic::Ordering::SeqCst),
            self.seq_out.load(std::sync::atomic::Ordering::SeqCst),
        )
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

    /// Remotes reported by the broker for this session ("host:port").
    /// The UDP relay forwards to `remotes[0]`, the same endpoint as TCP
    /// channel 1.
    pub fn remotes(&self) -> &[String] {
        &self.remotes
    }

    /// Install the UDP leg state. Must happen before the session is
    /// handed to `SessionManager::add_session`, which is what registers
    /// the token with the relay.
    pub fn set_udp(&self, udp: UdpState) {
        *self.udp.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(udp));
    }

    pub fn udp(&self) -> Option<Arc<UdpState>> {
        self.udp.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Drop the UDP leg state (relay inactivity reaper). Returns the
    /// removed state so the caller can unregister the token. The TCP
    /// session keeps running unaffected.
    pub fn clear_udp(&self) -> Option<Arc<UdpState>> {
        self.udp.write().unwrap_or_else(|e| e.into_inner()).take()
    }

    pub fn shared_secret(&self) -> &SharedSecret {
        &self.shared_secret
    }

    /// Launcher-side traffic counters for this session. Streams and the
    /// UDP relay add payload bytes here as they forward them; the
    /// snapshot goes out with the broker stop notification.
    pub fn traffic(&self) -> Arc<TrafficCounters> {
        self.traffic.clone()
    }

    /// Claim the one-shot broker stop notification for this session.
    /// Returns the notify ticket plus the traffic snapshot on the first
    /// call for a session that has one; `None` afterwards or for
    /// sessions that never went through a broker start.
    pub fn take_broker_stop(&self) -> Option<(ticket::Ticket, u64, u64)> {
        let notify = self.broker_stop_ticket?;
        if self
            .broker_notified
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return None;
        }
        let (sent, recv) = self.traffic.snapshot();
        Some((notify, sent, recv))
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

    /// Builds the launcher-leg crypt pair sharing the session's live
    /// per-direction counters. Every caller (connect handshake, each
    /// launcher-facing stream, each recovery handshake) gets crypts whose
    /// nonce counters are the *same* atomics, so sequence numbers advance
    /// once per direction across stream replacements with no coordination.
    /// Builds the launcher-leg crypt pair sharing the session's live
    /// per-direction counters and rekeying under the session's own `k`.
    /// Every caller (connect handshake, each launcher-facing stream, each
    /// recovery handshake) gets crypts whose nonce counters are the *same*
    /// atomics, so sequence numbers advance once per direction across
    /// stream replacements with no coordination — and the epoch of any
    /// frame is decided by its own seq under the *same* session PRK, never
    /// by a per-stream clock or a re-read of the config.
    pub fn server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        let material = crypt::tunnel::derive_tunnel_material(&self.shared_secret, self.ticket())?;
        let rekeys = crypt::tunnel::TunnelRekeys::from_parts(
            self.rekey_prk.clone(),
            &material,
            self.rekey_log2,
        );
        Ok((
            crypt::Crypt::with_rekey(self.seq_in.clone(), rekeys.inbound),
            crypt::Crypt::with_rekey(self.seq_out.clone(), rekeys.outbound),
        ))
    }

    pub(super) async fn fail_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.fail_server().await;
    }

    pub(super) async fn stop_client(&self, stream_channel_id: u16, generation: u64) {
        self.session_proxy
            .stop_client(stream_channel_id, generation)
            .await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.trigger();

        // Last chance to tell the broker this tunnel is over. By the time
        // the last Arc<Session> dies every launcher-facing task has
        // finished (they all hold clones or stop on `stop`), so the
        // traffic snapshot is complete.
        if let Some((notify, sent, recv)) = self.take_broker_stop() {
            crate::broker::spawn_stop_notification(notify, sent, recv);
        }
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

    /// Simulates the launcher side of the tunnel leg: a crypt with the key
    /// the session's *inbound* crypt decrypts with, and its own private
    /// sequence counter (the launcher's counters are independent of the
    /// session's by construction; only the server-side holders share them).
    fn launcher_side_crypt(session: &Session, seq: u64) -> crypt::Crypt {
        let material =
            crypt::tunnel::derive_tunnel_material(session.shared_secret(), session.ticket())
                .unwrap();
        crypt::Crypt::new(&material.key_receive, seq)
    }

    /// Session-pinned `k` must survive crypt rebuilds (the stream-
    /// replacement / recovery path): a mirror pair built through the public
    /// factory with the session's `k` decrypts every frame the session's
    /// outbound encrypted across epoch boundaries — proving the rebuilt
    /// crypts agree on `epoch(seq)` without re-reading anything.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn session_k_survives_crypt_rebuild_across_epochs() {
        let session = Session::with_rekey_log2(
            SharedSecret::new([0x77u8; 32]),
            ticket::Ticket::new_random(),
            Trigger::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
            None,
            2, // epoch rotates every 4 frames
        );
        assert_eq!(session.rekey_log2(), 2);

        let mut outbound = session.server_tunnel_crypts().unwrap().1;
        let mut mirror = crypt::tunnel::get_tunnel_crypts(
            session.shared_secret(),
            session.ticket(),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            session.rekey_log2(),
        )
        .unwrap();

        // 12 frames span epochs 0, 1, 2. Each one decrypts through the
        // mirror's s2c side (same PRK, same k, same dir — the launcher's
        // view of the session's key schedule).
        for i in 1..=12u64 {
            let payload = format!("k-{i}");
            let mut buf = crypt::types::PacketBuffer::new();
            buf.set_data(payload.as_bytes()).unwrap();
            outbound.encrypt(1, payload.len(), &mut buf).unwrap();
            mirror
                .1
                .decrypt(&mut buf)
                .unwrap_or_else(|e| panic!("rebuilt crypt must authenticate frame {i}: {e}"));
            assert_eq!(buf.data(), payload.as_bytes());
        }
        assert_eq!(session.seqs().1, 12);
    }

    /// Design doc test 7: a session running with a small `k` (4) relays
    /// `2^k + m` frames in BOTH directions across the epoch boundary and
    /// nothing breaks. Default sessions (k = 20) never reach an epoch in
    /// the test suite; this pins the machinery with a config-realistic
    /// small threshold.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn session_relays_across_epoch_boundary_with_small_k() {
        let session = Session::with_rekey_log2(
            SharedSecret::new([0x88u8; 32]),
            ticket::Ticket::new_random(),
            Trigger::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
            None,
            4, // epoch rotates every 16 frames
        );

        let (mut s_in, mut s_out) = session.server_tunnel_crypts().unwrap();
        // The launcher pair: mirror of the session's key schedule (server-
        // perspective factory; `inbound` is the launcher's *send* side —
        // same key/direction domain as the server's inbound decrypts).
        let (mut l_send, mut l_recv) = crypt::tunnel::get_tunnel_crypts(
            session.shared_secret(),
            session.ticket(),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            session.rekey_log2(),
        )
        .unwrap();

        // 40 server->launcher frames (epochs 0, 1, 2).
        for i in 1..=40u64 {
            let payload = format!("e2e-s2c-{i}");
            let mut buf = crypt::types::PacketBuffer::new();
            buf.set_data(payload.as_bytes()).unwrap();
            s_out.encrypt(3, payload.len(), &mut buf).unwrap();
            l_recv
                .decrypt(&mut buf)
                .unwrap_or_else(|e| panic!("s2c frame {i} (epoch {}): {e}", (i - 1) >> 4));
            assert_eq!(buf.data(), payload.as_bytes());
        }

        // 40 launcher->server frames across the same boundaries.
        for i in 1..=40u64 {
            let payload = format!("e2e-c2s-{i}");
            let mut buf = crypt::types::PacketBuffer::new();
            buf.set_data(payload.as_bytes()).unwrap();
            l_send.encrypt(3, payload.len(), &mut buf).unwrap();
            s_in.decrypt(&mut buf)
                .unwrap_or_else(|e| panic!("c2s frame {i} (epoch {}): {e}", (i - 1) >> 4));
            assert_eq!(buf.data(), payload.as_bytes());
        }

        // decrypt advances via fetch_max(last_used + 1): the last c2s frame
        // was 40, so the inbound counter sits at 41; 40 frames were
        // encrypted on the outbound side.
        assert_eq!(session.seqs(), (41, 40));
    }

    /// `k` outside the shift-safe bound is clamped, never propagated: a
    /// session built with e.g. 200 must behave as a `k = 63` session
    /// (`seq >> k` for a larger `k` would be undefined at the crypt).
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn session_clamps_out_of_range_k() {
        let session = Session::with_rekey_log2(
            SharedSecret::new([0x99u8; 32]),
            ticket::Ticket::new_random(),
            Trigger::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
            None,
            200,
        );
        assert_eq!(session.rekey_log2(), crypt::rekey::MAX_REKEY_LOG2);
        // ... and its crypts still work (single epoch for any real load).
        // Same-direction rebuild: a freshly built outbound crypt decrypts
        // the frame the session's outbound encrypted (both sit on the
        // session's s2c key domain).
        let mut outbound = session.server_tunnel_crypts().unwrap().1;
        let mut buf = crypt::types::PacketBuffer::new();
        buf.set_data(b"hi").unwrap();
        outbound.encrypt(1, 2, &mut buf).unwrap();
        session
            .server_tunnel_crypts()
            .unwrap()
            .1
            .decrypt(&mut buf)
            .unwrap();
    }

    /// The session's crypts share the session's live counters: every holder
    /// returned by `server_tunnel_crypts` moves the same `seq_in`/`seq_out`
    /// atomics when it encrypts/decrypts, so `seqs()` always reflects the
    /// true cryptographic state with no write-back step in between.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn seqs_are_the_shared_crypt_counters() {
        let session = new_test_session().await;
        assert_eq!(session.seqs(), (0, 0));

        let (mut inbound, mut outbound) = session.server_tunnel_crypts().unwrap();

        // encrypt pre-increments the shared outbound counter
        let mut buf = crypt::types::PacketBuffer::new();
        buf.set_data(b"abcd").unwrap();
        outbound.encrypt(1, 4, &mut buf).unwrap();
        assert_eq!(buf.seq().unwrap(), 1);
        assert_eq!(session.seqs().1, 1);

        // a *second* outbound holder continues from the shared state, it
        // cannot reuse the sequence number the first one consumed
        let (_, mut outbound2) = session.server_tunnel_crypts().unwrap();
        let mut buf2 = crypt::types::PacketBuffer::new();
        buf2.set_data(b"efgh").unwrap();
        outbound2.encrypt(1, 4, &mut buf2).unwrap();
        assert_eq!(buf2.seq().unwrap(), 2);
        assert_eq!(session.seqs().1, 2);

        // a launcher frame (encrypted with the key the session's inbound
        // decrypts) advances the shared inbound counter when decrypted
        let mut launcher = launcher_side_crypt(&session, 0);
        let mut frame = crypt::types::PacketBuffer::new();
        frame.set_data(b"ijkl").unwrap();
        launcher.encrypt(1, 4, &mut frame).unwrap();

        let mut inbound2 = session.server_tunnel_crypts().unwrap().0;
        let mut frame_copy = frame.clone();
        inbound2.decrypt(&mut frame_copy).unwrap();
        assert_eq!(session.seqs().0, 2); // fetch_max(last-used + 1)
        assert_eq!(inbound.current_seq(), 2);

        // the first holder sees the replay rejection through the shared
        // counter: the already-consumed frame is stale for *both* holders
        let mut frame_copy2 = frame;
        assert!(
            inbound
                .decrypt(&mut frame_copy2)
                .unwrap_err()
                .to_string()
                .contains("replay attack detected"),
            "shared counter must reject replays for every holder"
        );
    }

    /// `seqs()` exposes the two per-direction counters independently: a
    /// decrypt advances only `seq_in` and an encrypt only `seq_out`, so
    /// asymmetric pairs are the normal state, not corruption.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn seqs_tracks_advances_per_direction_independently() {
        let session = new_test_session().await;

        let (mut inbound, mut outbound) = session.server_tunnel_crypts().unwrap();

        // outbound: one encrypt -> seq_out 1
        let mut buf = crypt::types::PacketBuffer::new();
        buf.set_data(b"xy").unwrap();
        outbound.encrypt(1, 2, &mut buf).unwrap();

        // inbound: decrypt one launcher frame at seq 1 -> seq_in 2
        let mut launcher = launcher_side_crypt(&session, 0);
        let mut frame = crypt::types::PacketBuffer::new();
        frame.set_data(b"zw").unwrap();
        launcher.encrypt(1, 2, &mut frame).unwrap();
        let mut frame_copy = frame.clone();
        inbound.decrypt(&mut frame_copy).unwrap();
        // the launcher frame consumed seq 1 and the decrypt advanced to 2;
        // the *inbound* side of the session is the server's receive side,
        // which is independent of the server's outbound counter.
        assert_eq!(
            session.seqs(),
            (2, 1),
            "inbound and outbound counters advance independently"
        );
    }

    /// Replacing the server stream must kill the previous owner instantly:
    /// `start_server` takes the slot and fires the old owner's stop trigger
    /// before returning, with no drain wait.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn start_server_kills_previous_owner_immediately() {
        let session_ref = SessionManager::get_instance()
            .add_session(new_test_session().await)
            .expect("session id collision unlikely with random ticket");

        let (_endpoints, first) = session_ref.start_server().await.unwrap();
        assert!(session_ref.is_current_server_stream(&first));
        assert!(!first.stopper().is_triggered());

        let (_endpoints, second) = session_ref.start_server().await.unwrap();

        // the previous owner was killed and replaced; the new one owns the slot
        assert!(first.stopper().is_triggered());
        assert!(!session_ref.is_current_server_stream(&first));
        assert!(session_ref.is_current_server_stream(&second));
    }

    /// `kill_current_server_stream` empties the slot and triggers the stop,
    /// so a killed stream can never pass the `is_current_server_stream`
    /// teardown gate.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn kill_current_server_stream_detaches_owner() {
        let session_ref = SessionManager::get_instance()
            .add_session(new_test_session().await)
            .expect("session id collision unlikely with random ticket");

        let (_endpoints, owner) = session_ref.start_server().await.unwrap();
        session_ref.kill_current_server_stream();

        assert!(owner.stopper().is_triggered());
        assert!(!session_ref.is_current_server_stream(&owner));

        // killing an empty slot is a no-op
        session_ref.kill_current_server_stream();
    }

    /// `udp` starts as `None` on a fresh session, accepts a `UdpState`,
    /// and `clear_udp` removes it again while the session keeps running.
    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn udp_state_lifecycle_on_session() {
        let session = new_test_session().await;
        assert!(session.udp().is_none());

        let token = [0x55u8; 16];
        let key = SharedSecret::new([9u8; 32]);
        let (inbound, outbound) = (
            crypt::datagram::DatagramCrypt::new(&key),
            crypt::datagram::DatagramCrypt::new(&key),
        );
        session.set_udp(UdpState::new(token, inbound, outbound));

        let udp = session.udp().expect("udp state must be present");
        assert_eq!(udp.token, token);
        assert!(udp.client_addr().is_none());

        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        udp.set_client_addr(addr);
        assert_eq!(udp.client_addr(), Some(addr));

        let before = udp.last_activity();
        udp.touch();
        assert!(udp.last_activity() >= before);

        let removed = session.clear_udp().expect("clear must return the state");
        assert_eq!(removed.token, token);
        assert!(
            session.udp().is_none(),
            "udp state must be gone after clear"
        );
        // Clearing twice is a no-op
        assert!(session.clear_udp().is_none());
    }
    /// `current_equiv_id` starts as `None` on a fresh session, accepts
    /// arbitrary `Some(_)` writes, and accepts a clear back to `None`.
    /// This is the atomicity guarantee of the unit backing
    /// `SessionManager::get_equiv_session` / `create_equiv_session`.
    #[serial_test::serial(manager)]
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
    #[serial_test::serial(manager)]
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

    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn traffic_counters_accumulate_payload_bytes() {
        let session = new_test_session().await;
        let traffic = session.traffic();

        assert_eq!(traffic.snapshot(), (0, 0));
        traffic.add_sent(10);
        traffic.add_recv(4);
        traffic.add_sent(1);
        assert_eq!(traffic.snapshot(), (11, 4));
    }

    #[serial_test::serial(manager)]
    #[tokio::test]
    async fn take_broker_stop_is_one_shot_and_snapshots_traffic() {
        let notify = ticket::Ticket::new_random();
        let session = Session::with_broker_stop_ticket(
            SharedSecret::new([0u8; 32]),
            ticket::Ticket::new_random(),
            Trigger::new(),
            "127.0.0.1:0".parse().unwrap(),
            vec![],
            Some(notify),
        );

        session.traffic().add_sent(7);
        session.traffic().add_recv(3);

        assert_eq!(session.take_broker_stop(), Some((notify, 7, 3)));
        // Second claim must be a no-op: the broker stop report is
        // exactly-once per session, even with Drop + shutdown racing.
        assert!(session.take_broker_stop().is_none());

        // Sessions without a notify ticket never report.
        let plain = new_test_session().await;
        assert!(plain.take_broker_stop().is_none());
    }
}
