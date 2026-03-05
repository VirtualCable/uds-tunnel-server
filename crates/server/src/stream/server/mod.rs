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

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{
    crypt::{Crypt, types::PacketBuffer},
    log,
    protocol::{PayloadWithChannel, PayloadWithChannelReceiver, PayloadWithChannelSender},
    system::trigger::Trigger,
};

use crate::{
    consts::SERVER_RECOVERY_GRACE_SECS, // global crate consts
    session::{SessionId, SessionManager},
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    server_stop: Trigger,
    sender: PayloadWithChannelSender,
    buffer: PacketBuffer,
    crypt: Crypt,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelServerInboundStream<R> {
    pub fn new(reader: R, crypt: Crypt, sender: PayloadWithChannelSender, stop: Trigger) -> Self {
        TunnelServerInboundStream {
            server_stop: stop,
            sender,
            crypt,
            buffer: PacketBuffer::new(),
            reader,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting server inbound stream");

        loop {
            let (decrypted_data, stream_channel_id) = self
                .crypt
                .read(&self.server_stop, &mut self.reader, &mut self.buffer)
                .await?;
            if decrypted_data.is_empty() {
                log::debug!("Server inbound stream reached EOF");
                // Connection closed
                break;
            }
            // Channels are processed on the proxy side, so just forward data
            self.sender
                .send_async(PayloadWithChannel::new(stream_channel_id, decrypted_data))
                .await?;
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

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelServerOutboundStream<W> {
    pub fn new(
        writer: W,
        crypt: Crypt,
        receiver: PayloadWithChannelReceiver,
        stop: Trigger,
        session_id: SessionId,
    ) -> Self {
        TunnelServerOutboundStream {
            server_stop: stop,
            receiver,
            crypt,
            session_id,
            writer,
        }
    }

    async fn send_packet(&mut self, packet: PayloadWithChannel) -> Result<()> {
        if let Err(e) = self.send_data(&packet).await {
            log::error!("Error sending data in server outbound stream: {:?}", e);
            // Store in session so it can be resent if the stream is restarted due to a recoverable error
            SessionManager::get_instance().set_unsent_packets(&self.session_id, packet);
            return Err(e);
        }
        Ok(())
    }

    pub async fn run(&mut self) -> Result<()> {
        // If there are unset packets, sent them now
        if let Some(unsent_packet) =
            SessionManager::get_instance().get_unsent_packets(&self.session_id)
        {
            log::debug!(
                "Resending unsent packet for session {:?} in server outbound stream",
                self.session_id
            );
            self.send_packet(unsent_packet).await?;
        }

        loop {
            tokio::select! {
                _ = self.server_stop.wait_async() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(channel_data) => {
                            self.send_packet(channel_data).await?;
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
            .write(
                &self.server_stop,
                &mut self.writer,
                data.channel_id,
                data.payload.as_ref(),
            )
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
                session.stop_trigger(),
                session.start_server().await?,
                inbound_crypt,
                outbound_crypt,
            )
        };

        let server_stop = Trigger::new();

        let inbound =
            TunnelServerInboundStream::new(reader, inbound_crypt, channels.tx, server_stop.clone());

        let outbound = TunnelServerOutboundStream::new(
            writer,
            outbound_crypt,
            channels.rx,
            server_stop.clone(),
            session_id,
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
                    "Server tunnel streams ended normally for session {:?}",
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

        // Store back seqs on session, so if client recovers, it can continue with correct seq numbers
        if let Some(session) = session_manager.get_session(&session_id) {
            session.set_inbound_seq(inbound.crypt.current_seq());
            session.set_outbound_seq(outbound.crypt.current_seq());
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
