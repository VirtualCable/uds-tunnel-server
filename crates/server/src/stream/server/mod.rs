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
    consts::SERVER_RECOVERY_GRACE_SECS, // global crate consts
    session::{SessionId, SessionManager, TrafficCounters},
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    session_id: SessionId,
    server_stop: Trigger,
    sender: PayloadWithChannelSender,
    buffer: PacketBuffer,
    crypt: Crypt,
    traffic: Arc<TrafficCounters>,

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
            reader,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting server inbound stream");

        loop {
            tokio::select! {
                biased;
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
                        if stream_channel_id == 0 {
                            // The CLOSE command is processed here, as we need to do it BEFORE the EOF
                            if let Ok(cmd) = protocol::Command::from_slice(decrypted_data)
                                && cmd == protocol::Command::Close
                            {
                                log::debug!("Received CLOSE command on server inbound stream");
                                // Notify session manager that close was notified, so it can skip recovery grace period and close immediately
                                SessionManager::get_instance().close_notified(&self.session_id);
                                break;
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
        // leaves the packet buffered for the next recovery attempt). The
        // buffer was just drained empty, so these pushes cannot evict or fail.
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
                            // Store on recovery buffer, so if we fail to send, we can retry on next connection.
                            // The buffer is behind a Mutex, so we clone the payload here and release the
                            // lock before sending; the item stored in the buffer remains valid for the
                            // next recover replay.
                            let to_send = {
                                let mut buf = recovery_buffer.lock();
                                let stored = buf.push(self.crypt.current_seq() + 1, channel_data)?;
                                stored.clone()
                            };
                            self.send_data(&to_send).await?;
                            if to_send.channel_id != 0 {
                                // Download to the launcher: payload bytes
                                // only (channel 0 is control traffic), and
                                // only counted here — recovery re-sends are
                                // not re-counted, at the cost of undercounting
                                // the rare packet that first lands through
                                // `recover_buffer`. Fine for informational
                                // broker stats.
                                self.traffic.add_recv(to_send.payload.len() as u64);
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

    pub async fn run(self) -> Result<()> {
        let Self {
            session_id,
            reader,
            writer,
        } = self;

        let session_manager = SessionManager::get_instance();
        let session = if let Some(session) = session_manager.get_session(&session_id) {
            session
        } else {
            log::warn!("Session {:?} not found, aborting stream", session_id);
            return Ok(());
        };

        let (stop, channels, inbound_crypt, outbound_crypt) = {
            let (inbound_crypt, outbound_crypt) = session.server_tunnel_crypts()?;
            (
                session.stopper(),
                session.start_server().await?,
                inbound_crypt,
                outbound_crypt,
            )
        };

        let server_stop = Trigger::new();
        let traffic = session.traffic();

        let inbound = TunnelServerInboundStream::new(
            reader,
            inbound_crypt,
            channels.tx,
            server_stop.clone(),
            session_id,
            traffic.clone(),
        );

        let outbound = TunnelServerOutboundStream::new(
            writer,
            outbound_crypt,
            channels.rx,
            server_stop.clone(),
            session_id,
            traffic,
        );

        tokio::spawn({
            let server_stop = server_stop.clone();
            async move {
                if let Err(e) = Self::run_streams(session_id, inbound, outbound, server_stop).await
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

        Ok(())
    }

    async fn run_streams(
        session_id: SessionId,
        mut inbound: TunnelServerInboundStream<R>,
        mut outbound: TunnelServerOutboundStream<W>,
        server_stop: Trigger,
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

        // Store back seqs on session, so if client recovers, it can continue with correct seq numbers
        if let Some(session) = session_manager.get_session(&session_id) {
            session.set_seqs(inbound_seq, outbound_seq);
        }

        if session_manager.is_close_notified(&session_id) {
            // Close correctly notified
            session_manager.stop_server(&session_id).await;
        } else {
            // Notify failed to drop server side
            session_manager.fail_server(&session_id).await;

            // Give a chance to recover before stopping session, as some errors might be transient and recoverable by the client
            tokio::time::sleep(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS)).await;
            if let Some(session) = session_manager.get_session(&session_id) {
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
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
