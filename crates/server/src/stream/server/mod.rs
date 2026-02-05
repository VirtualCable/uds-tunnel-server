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

use std::sync::{Arc, atomic::AtomicBool};

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{
    crypt::{Crypt, consts::CRYPT_PACKET_SIZE, types::PacketBuffer},
    log,
    protocol::{PayloadWithChannel, PayloadWithChannelReceiver, PayloadWithChannelSender},
    system::trigger::Trigger,
};

use crate::{
    consts::SERVER_RECOVERY_GRACE_SECS, // global crate consts
    session::{SessionId, SessionManager},
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    stop: Trigger,
    sender: PayloadWithChannelSender,
    buffer: PacketBuffer,
    crypt: Crypt,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelServerInboundStream<R> {
    pub fn new(reader: R, crypt: Crypt, sender: PayloadWithChannelSender, stop: Trigger) -> Self {
        TunnelServerInboundStream {
            stop,
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
                .read(&self.stop, &mut self.reader, &mut self.buffer)
                .await?;
            if decrypted_data.is_empty() {
                log::debug!("Server inbound stream reached EOF");
                // Connection closed
                break;
            }
            // Channels are processed on the proxy side, so just forward data
            self.sender
                .try_send(PayloadWithChannel::new(stream_channel_id, decrypted_data))?;
        }
        Ok(())
    }
}

struct TunnelServerOutboundStream<W: AsyncWriteExt + Unpin> {
    stop: Trigger,
    receiver: PayloadWithChannelReceiver,
    crypt: Crypt,

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelServerOutboundStream<W> {
    pub fn new(writer: W, crypt: Crypt, receiver: PayloadWithChannelReceiver, stop: Trigger) -> Self {
        TunnelServerOutboundStream {
            stop,
            receiver,
            crypt,
            writer,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.stop.wait_async() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(channel_data) => {
                            self.send_data(channel_data).await?
                        }
                        Err(e) => {
                            // Maybe the receiver "won" the select! but stop is already set. This is fine
                            if self.stop.is_triggered() {
                                break;
                            }
                            log::error!("Server outbound receiver channel closed: {:?}", e);
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn send_data(&mut self, data: PayloadWithChannel) -> Result<()> {
        let mut offset = 0;

        let payload = data.payload.as_ref();
        // Divide data into CRYPT_PACKET_SIZE chunks and send them
        while offset < payload.len() {
            let end = (offset + CRYPT_PACKET_SIZE).min(payload.len());
            let chunk = &payload[offset..end];
            self.crypt.write(&mut self.writer, data.channel_id, chunk).await?;
            offset = end;
        }

        Ok(())
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

        let (stop, channels, inbound_crypt, outbound_crypt) =
            if let Some(session) = session_manager.get_session(&session_id) {
                let (inbound_crypt, outbound_crypt) = session.server_tunnel_crypts()?;
                (
                    session.stop_trigger(),
                    session.start_server().await?,
                    inbound_crypt,
                    outbound_crypt,
                )
            } else {
                log::warn!("Session {:?} not found, aborting stream", session_id);
                return Ok(());
            };

        let local_stop = Trigger::new();

        let mut inbound =
            TunnelServerInboundStream::new(reader, inbound_crypt, channels.tx, local_stop.clone());

        let mut outbound = TunnelServerOutboundStream::new(
            writer,
            outbound_crypt,
            channels.rx,
            local_stop.clone(),
        );

        let tunnel_error = Arc::new(AtomicBool::new(false));
        tokio::spawn({
            let tunnel_error = tunnel_error.clone();
            async move {
                if let Err(e) = inbound.run().await {
                    log::error!("Inbound stream error: {:?}", e);
                    tunnel_error.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // let's ensure the other side is also stopped
                inbound.stop.trigger();
            }
        });

        tokio::spawn({
            let tunnel_error = tunnel_error.clone();
            async move {
                if let Err(e) = outbound.run().await {
                    log::error!("Outbound stream error: {:?}", e);
                    tunnel_error.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                // let's ensure the other side is also stopped
                outbound.stop.trigger();
            }
        });

        tokio::spawn(async move {
            tokio::select! {
                _ = stop.wait_async() => {
                    local_stop.trigger();
                }
                _ = local_stop.wait_async() => {}
            }

            let ended_with_error = tunnel_error.load(std::sync::atomic::Ordering::Relaxed);
            if ended_with_error {
                log::debug!(
                    "Server stream for session {:?} stopping due to error",
                    session_id
                );
                // Notify failing server side
                session_manager.fail_server(&session_id).await;

                // Wait a bit for recovery grace period
                tokio::time::sleep(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS))
                    .await;
                if let Some(session) = session_manager.get_session(&session_id) {
                    if session.is_server_running() {
                        log::debug!(
                            "Session {:?} is still running after error grace period, not stopping",
                            session_id
                        );
                        return;
                    }
                    log::debug!("Stopping session {:?} after error grace period", session_id);
                    // Notify stopping server side, will stop proxy and remove session
                    session_manager.stop_server(&session_id).await;
                }
            } else {
                log::debug!(
                    "Server stream for session {:?} stopping normally",
                    session_id
                );
                // Notify stopping server side
                session_manager.stop_server(&session_id).await;
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
