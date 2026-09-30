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

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{
    crypt::{Crypt, types::PacketBuffer},
    log,
    protocol::{self, PayloadWithChannel, PayloadWithChannelReceiver, PayloadWithChannelSender},
    system::trigger::Trigger,
};

use crate::{
    consts::{KEEPALIVE_TIMEOUT_SECS, SERVER_RECOVERY_GRACE_SECS}, // global crate consts
    session::{
        ServerEndpoints, ServerStreamOwner, Session, SessionId, SessionManager, TrafficCounters,
    },
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    session_id: SessionId,
    server_stop: Trigger,
    sender: PayloadWithChannelSender,
    buffer: PacketBuffer,
    crypt: Crypt,
    traffic: Arc<TrafficCounters>,
    // Wall clock (tokio virtual-time aware) of the last frame decrypted from
    // the launcher, whatever its channel. Any traffic refreshes the
    // keep-alive deadline; the `Nop` control command exists so an idle
    // tunnel keeps it refreshed too. If no frame arrives within
    // KEEPALIVE_TIMEOUT_SECS the leg is declared dead: without one, a
    // half-open TCP socket (client vanished without RST/FIN) can take the
    // OS retransmission timeout -- minutes -- to report, pinning a
    // session's streams and proxy channels long after the tunnel is gone.
    last_frame: tokio::time::Instant,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelServerInboundStream<R> {
    pub fn new(
        reader: R,
        crypt: Crypt,
        sender: PayloadWithChannelSender,
        stop: Trigger,
        session_id: SessionId,
        traffic: Arc<TrafficCounters>,
    ) -> Self {
        TunnelServerInboundStream {
            session_id,
            server_stop: stop,
            sender,
            crypt,
            buffer: PacketBuffer::new(),
            traffic,
            last_frame: tokio::time::Instant::now(),
            reader,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting server inbound stream");

        let keepalive = std::time::Duration::from_secs(KEEPALIVE_TIMEOUT_SECS);
        loop {
            let idle_for = self.last_frame.elapsed();
            // Sleep only as long as the deadline actually leaves, so the
            // watchdog fires promptly after a quiet stretch instead of
            // busy-looping on an already-expired clock.
            let watchdog = tokio::time::sleep(keepalive.saturating_sub(idle_for));
            tokio::select! {
                biased;  // stop first; reads before the watchdog, so a frame
                         // already in the socket buffer always refreshes the clock
                _ = self.server_stop.wait_async() => {
                    log::debug!("Server inbound stream stopping");
                    break;
                }
                result = self
                    .crypt
                    .read(&mut self.reader, &mut self.buffer)
                    => {
                        let (decrypted_data, stream_channel_id) = result?;
                        if decrypted_data.is_empty() {
                            log::debug!("Server inbound stream reached EOF");
                            // Connection closed
                            break;
                        }
                        // Any well-formed decrypted frame from the launcher
                        // proves the leg is alive, whatever the channel.
                        self.last_frame = tokio::time::Instant::now();
                        if stream_channel_id == 0 {
                            // The CLOSE command is processed here, as we need to do it BEFORE the EOF
                            if let Ok(cmd) = protocol::Command::from_slice(decrypted_data) {
                                match cmd {
                                    protocol::Command::Close => {
                                        log::debug!("Received CLOSE command on server inbound stream");
                                        // Notify session manager that close was notified, so it can skip recovery grace period and close immediately
                                        SessionManager::get_instance().close_notified(&self.session_id);
                                        break;
                                    }
                                    // A keep-alive is consumed here; forwarding it
                                    // to the proxy would hit its "unexpected
                                    // command" arm and tear the session down.
                                    protocol::Command::Nop => {
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // Channels are processed on the proxy side, so just forward data
                        if stream_channel_id != 0 {
                            // Client upload: count payload bytes for the
                            // broker stop report. Channel 0 is control
                            // traffic and is not tunnel payload.
                            self.traffic.add_sent(decrypted_data.len() as u64);
                        }
                        self.sender
                            .send_async(PayloadWithChannel::new(stream_channel_id, decrypted_data))
                            .await?;
                }
                _ = watchdog => {
                    if self.last_frame.elapsed() >= keepalive {
                        log::warn!(
                            "Launcher keep-alive timeout on session {:?}: no frame for {}s, ending stream",
                            self.session_id,
                            KEEPALIVE_TIMEOUT_SECS,
                        );
                        // Treating the leg as dead runs the same teardown as any
                        // stream end (fail_server -> recovery grace), so a
                        // launcher that is merely slow-but-connected can still
                        // recover within the grace; an unrecoverable half-open
                        // socket does not, and the session is then freed.
                        break;
                    }
                    // Traffic refreshed the clock between scheduling and
                    // firing; recompute and keep waiting.
                    continue;
                }
            }
        }
        // Ensure other side also stops
        self.server_stop.trigger();
        Ok(())
    }
}

struct TunnelServerOutboundStream<W: AsyncWriteExt + Unpin> {
    server_stop: Trigger,
    receiver: PayloadWithChannelReceiver,
    crypt: Crypt,
    session_id: SessionId,
    traffic: Arc<TrafficCounters>,

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelServerOutboundStream<W> {
    pub fn new(
        writer: W,
        crypt: Crypt,
        receiver: PayloadWithChannelReceiver,
        stop: Trigger,
        session_id: SessionId,
        traffic: Arc<TrafficCounters>,
    ) -> Self {
        TunnelServerOutboundStream {
            server_stop: stop,
            receiver,
            crypt,
            session_id,
            traffic,
            writer,
        }
    }

    pub async fn recover_buffer(&mut self) -> Result<()> {
        let recovery_buffer =
            SessionManager::get_instance().get_recovery_buffer(&self.session_id)?;

        log::debug!(
            "Resending unsent packet for session {:?} in server outbound stream",
            self.session_id
        );
        // Drain the buffer into a local vec under the lock, then release it
        // before any .await. Holding the MutexGuard across an await would
        // also make the future non-Send (the guard is !Send).
        let unsent: Vec<(PayloadWithChannel, u64)> = {
            let mut buf = recovery_buffer.lock();
            let mut drained = Vec::new();
            while let Some(item) = buf.take_unsent_packet() {
                drained.push(item);
            }
            drained
        };

        // Send in buffer (FIFO) order. If any send fails, re-queue the failed
        // item and everything still pending behind it, mirroring the
        // steady-state invariant in `run` (push-then-send: a send failure
        // leaves the packet buffered for the next recovery attempt).
        //
        // These re-pushes cannot fail the capacity check: every item here
        // coexisted in this same buffer before the drain, so each one's
        // length is within `max_bytes` by construction. They also cannot
        // evict anything: the re-pushed set totals at most the bytes the
        // buffer held before the drain, and the drain happened atomically
        // under the per-session mutex. Both halves hold even if a replaced
        // (killed) stream is still parked mid-send on its own drained items
        // — those items are privately owned by that stream's re-push path,
        // never double-handed. What the kill-on-attach single-live-stream
        // invariant (`Session::start_server`) adds is that no *other* live
        // producer can push fresh frames into the buffer between this
        // drain and these re-pushes and consume the freed capacity; without
        // it, the eviction loop here could fire against a peer stream's
        // packets.
        let mut iter = unsent.into_iter();
        while let Some((unsent_packet, old_seq)) = iter.next() {
            log::debug!(
                "Resend old seq {} len {}: {:?}..{:?}",
                old_seq,
                unsent_packet.len(),
                unsent_packet.payload.as_ref()[..std::cmp::min(8, unsent_packet.payload.len())]
                    .to_vec(),
                unsent_packet.payload.as_ref()[unsent_packet.payload.len().saturating_sub(8)..]
                    .to_vec(),
            );
            if let Err(e) = self.send_data(&unsent_packet).await {
                let mut buf = recovery_buffer.lock();
                let _ = buf.push(old_seq, unsent_packet); // drained buffer: cannot fail
                for (pending, pending_seq) in iter.by_ref() {
                    let _ = buf.push(pending_seq, pending);
                }
                return Err(e);
            }
        }
        log::debug!(
            "Finished resending unsent packets for session {:?} in server outbound stream",
            self.session_id
        );
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        self.recover_buffer().await?;

        let recovery_buffer =
            SessionManager::get_instance().get_recovery_buffer(&self.session_id)?;

        loop {
            tokio::select! {
                biased;  // No random, first stop and then receiver
                _ = self.server_stop.wait_async() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(channel_data) => {
                            // Encrypt the frame *before* stamping it: the
                            // recovery-buffer label must be the sequence the
                            // frame actually carries on the wire. A
                            // prediction taken before the encrypt
                            // (`current_seq() + 1`) desynchronizes from the
                            // wire as soon as another holder of the shared
                            // session counter advances it between the read
                            // and the encrypt (e.g. a recovery handshake
                            // overlapping this, still-live stream), and a
                            // later recovery then mis-skips or refuses the
                            // window.
                            let channel_id = channel_data.channel_id;
                            let payload_len = channel_data.payload.len();
                            let mut buffer = PacketBuffer::from(channel_data.payload.as_ref());
                            self.crypt.encrypt(channel_id, payload_len, &mut buffer)?;
                            let seq = buffer.seq()?;
                            // Store on recovery buffer, so if we fail to send, we can retry on next connection.
                            // The buffer is behind a Mutex, so we move the payload in and release the
                            // lock before sending; the item stored in the buffer remains valid for the
                            // next recover replay.
                            {
                                let mut buf = recovery_buffer.lock();
                                buf.push(seq, channel_data)?;
                            }
                            buffer.write(&mut self.writer).await?;
                            if channel_id != 0 {
                                // Download to the launcher: payload bytes
                                // only (channel 0 is control traffic), and
                                // only counted here — recovery re-sends are
                                // not re-counted, at the cost of undercounting
                                // the rare packet that first lands through
                                // `recover_buffer`. Fine for informational
                                // broker stats.
                                self.traffic.add_recv(payload_len as u64);
                            }
                        }
                        Err(e) => {
                            // Maybe the receiver "won" the select! but stop is already set. This is fine
                            if self.server_stop.is_triggered() {
                                break;
                            }
                            log::error!("Server outbound receiver channel closed: {:?}", e);
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
        }
        self.server_stop.trigger();
        Ok(())
    }

    async fn send_data(&mut self, data: &PayloadWithChannel) -> Result<()> {
        self.crypt
            .write(&mut self.writer, data.channel_id, data.payload.as_ref())
            .await
    }
}

/// Runs a tunnel stream with inbound and outbound processing
/// # Arguments
/// * `stream` - The TCP stream to handle
/// * `inbound_crypt` - Crypt object for inbound data decryption
/// * `inbound_channel` - Receiver channel for inbound data (from Server side)
/// * `outbound_crypt` - Crypt object for outbound data encryption
/// * `outbound_channel` - Sender channel for outbound data (to Server side)
/// * `stop` - Trigger to stop the stream
/// # Returns
/// Nothing, runs indefinitely until stopped
///
/// Note: "Server side" is the side that communicates with the remote Server
pub struct TunnelServerStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    session_id: SessionId,
    reader: R,
    writer: W,
}

impl<R, W> TunnelServerStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    pub fn new(session_id: SessionId, reader: R, writer: W) -> Self {
        Self {
            session_id,
            reader,
            writer,
        }
    }

    /// Attach a *newly* opened launcher connection (full `Open` handshake):
    /// run the session's attach sequence (kill any predecessor stream,
    /// allocate the proxy channel set, mint the owner record) and spawn the
    /// pump task.
    ///
    /// The attach runs under the session's attach lock, the same critical
    /// section the recovery path uses, so a kill -> reseed -> attach on one
    /// side can never interleave with this kill -> attach on the other.
    pub async fn run(self) -> Result<()> {
        let session_manager = SessionManager::get_instance();
        let session = if let Some(session) = session_manager.get_session(&self.session_id) {
            session
        } else {
            log::warn!("Session {:?} not found, aborting stream", self.session_id);
            return Ok(());
        };

        let _attach_guard = session.lock_server_attach().await;
        let (endpoints, owner) = session.start_server().await?;
        self.run_attached(session.clone(), endpoints, owner);
        Ok(())
    }

    /// Run the pump over an *already attached* proxy channel set (`endpoints`)
    /// and its owner record. The caller (the recovery handshake) has already
    /// completed the `OpenResponse` exchange and holds the attach lock, so no
    /// proxy mutation happens here; this only drives I/O until the owner is
    /// stopped or replaced. Spawned onto the task scheduler.
    pub(crate) fn run_attached(
        self,
        session: Arc<Session>,
        endpoints: ServerEndpoints,
        owner: Arc<ServerStreamOwner>,
    ) {
        let Self {
            session_id,
            reader,
            writer,
        } = self;

        let (inbound_crypt, outbound_crypt) = match session.server_tunnel_crypts() {
            Ok(crypts) => crypts,
            Err(e) => {
                log::error!("Failed to build server tunnel crypts: {:?}", e);
                return;
            }
        };

        let stop = session.stopper();
        let server_stop = owner.stopper();
        let traffic = session.traffic();

        let inbound = TunnelServerInboundStream::new(
            reader,
            inbound_crypt,
            endpoints.tx,
            server_stop.clone(),
            session_id,
            traffic.clone(),
        );

        let outbound = TunnelServerOutboundStream::new(
            writer,
            outbound_crypt,
            endpoints.rx,
            server_stop.clone(),
            session_id,
            traffic,
        );

        tokio::spawn({
            let owner = owner.clone();
            let server_stop = server_stop.clone();
            async move {
                if let Err(e) =
                    Self::run_streams(session_id, inbound, outbound, server_stop, owner).await
                {
                    log::error!(
                        "Error running tunnel server stream for session {:?}: {:?}",
                        session_id,
                        e
                    );
                }
            }
        });

        tokio::spawn(async move {
            tokio::select! {
                _ = stop.wait_async() => {
                    server_stop.trigger();
                }
                _ = server_stop.wait_async() => {}
            }
        });
    }

    async fn run_streams(
        session_id: SessionId,
        mut inbound: TunnelServerInboundStream<R>,
        mut outbound: TunnelServerOutboundStream<W>,
        server_stop: Trigger,
        owner: Arc<ServerStreamOwner>,
    ) -> Result<()> {
        let session_manager = SessionManager::get_instance();

        match tokio::try_join!(inbound.run(), outbound.run()) {
            Ok(_) => {
                log::debug!(
                    "Server tunnel streams without errors on session {:?}",
                    outbound.session_id
                );
            }
            Err(e) => {
                // On error, the other side could have not set the stop trigger
                server_stop.trigger();

                log::error!(
                    "Error in server tunnel streams for session {:?}: {:?}",
                    outbound.session_id,
                    e
                );
            }
        }
        let (inbound_seq, outbound_seq) =
            (inbound.crypt.current_seq(), outbound.crypt.current_seq());
        log::debug!(
            "Server tunnel streams ended for session {:?}, inbound_seq: {}, outbound_seq: {}",
            session_id,
            inbound_seq,
            outbound_seq
        );

        let Some(session) = session_manager.get_session(&session_id) else {
            // Session gone: nothing to tear down.
            return Ok(());
        };

        // Serialize the proxy teardown against the attach path: take the
        // lock and check that no recovery replaced us. A replaced stream
        // must not fail/stop the attachment the new stream owns (the proxy
        // keeps a single server-side channel set), so it exits here. Its
        // crypts already shared the session counters, so the in-flight
        // sequence numbers it consumed could not collide with the new
        // stream's.
        let _attach_guard = session.lock_server_attach().await;
        if !session.is_current_server_stream(&owner) {
            log::debug!(
                "Server stream for session {:?} replaced, skipping proxy teardown",
                session_id
            );
            return Ok(());
        }

        if session_manager.is_close_notified(&session_id) {
            // Close correctly notified
            session_manager.stop_server(&session_id).await;
        } else {
            // Notify failed to drop server side
            session_manager.fail_server(&session_id).await;

            // Give a chance to recover before stopping session, as some
            // errors might be transient and recoverable by the client. The
            // attach lock is released while sleeping: a recovery handshake
            // (or a fresh Open attach) that arrives during the grace must be
            // able to take the session over; otherwise the grace window this
            // teardown exists for could never be used.
            drop(_attach_guard);
            tokio::time::sleep(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS)).await;

            let Some(session) = session_manager.get_session(&session_id) else {
                return Ok(());
            };
            let _attach_guard = session.lock_server_attach().await;
            // If a recovery or a fresh Open took the session over during the
            // grace period, its stream owns the proxy now: stopping the
            // server (or the session) here would tear down the live tunnel.
            if !session.is_current_server_stream(&owner) {
                log::debug!(
                    "Session {:?} taken over during recovery grace, not stopping",
                    session_id
                );
                return Ok(());
            }
            if session.is_server_running() {
                log::debug!(
                    "Session {:?} is still running after error grace period, not stopping",
                    session_id
                );
                return Ok(());
            }
            log::debug!("Stopping session {:?} after error grace period", session_id);
            // Notify stopping server side, will stop proxy and remove session
            session_manager.stop_server(&session_id).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tests_inbound_edge_cases;
