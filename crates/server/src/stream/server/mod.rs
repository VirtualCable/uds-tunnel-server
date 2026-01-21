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

// Authors: Adolfo Gómez, dkmaster at dkmon dot compub mod broker;

use anyhow::Result;
use flume::{Receiver, Sender};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    consts::SERVER_RECOVERY_GRACE_SECS, // global crate consts
    crypt::{
        Crypt, build_header,
        consts::{CRYPT_PACKET_SIZE, HEADER_LENGTH},
        types::PacketBuffer,
    },
    log,
    session::{SessionId, SessionManager},
    system::trigger::Trigger,
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    stop: Trigger,
    sender: Sender<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelServerInboundStream<R> {
    pub fn new(reader: R, crypt: Crypt, sender: Sender<Vec<u8>>, stop: Trigger) -> Self {
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
            let decrypted_data = self
                .crypt
                .read(&self.stop, &mut self.reader, &mut self.buffer)
                .await?;
            if decrypted_data.is_empty() {
                log::debug!("Server inbound stream reached EOF");
                // Connection closed
                break;
            }
            self.sender.try_send(decrypted_data)?;
        }
        Ok(())
    }
}

struct TunnelServerOutboundStream<W: AsyncWriteExt + Unpin> {
    stop: Trigger,
    receiver: Receiver<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelServerOutboundStream<W> {
    pub fn new(writer: W, crypt: Crypt, receiver: Receiver<Vec<u8>>, stop: Trigger) -> Self {
        TunnelServerOutboundStream {
            stop,
            receiver,
            crypt,
            buffer: PacketBuffer::new(),
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
                        Ok(data) => {
                            self.send_data(&data).await?
                        }
                        Err(_) => {
                            // Maybe the receiver "won" the select! but stop is already set. This is fine
                            if self.stop.is_triggered() {
                                break;
                            }
                            log::error!("Server outbound receiver channel closed");
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn send_packet(&mut self, chunk: &[u8]) -> Result<()> {
        // Copy data to buffer
        let buf = self.buffer.as_mut_slice();
        buf[..chunk.len()].copy_from_slice(chunk);

        // Crypt
        let encrypted = self.crypt.encrypt(chunk.len(), &mut self.buffer)?;

        // Header
        let counter = self.crypt.current_seq();
        let length = encrypted.len() as u16;

        let mut header = [0u8; HEADER_LENGTH];
        build_header(counter, length, &mut header)?;

        // Send
        self.writer.write_all(&header).await?;
        self.writer.write_all(encrypted).await?;

        Ok(())
    }

    async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + CRYPT_PACKET_SIZE).min(data.len());
            let chunk = &data[offset..end];

            self.send_packet(chunk).await?;
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
                let (inbound_crypt, outbound_crypt) = session.get_server_tunnel_crypts()?;
                (
                    session.get_stop_trigger(),
                    session.get_server_channels().await?,
                    inbound_crypt,
                    outbound_crypt,
                )
            } else {
                log::warn!("Session {:?} not found, aborting stream", session_id);
                return Ok(());
            };

        let local_stop = Trigger::new();

        let mut inbound =
            TunnelServerInboundStream::new(reader, inbound_crypt, channels.0, local_stop.clone());

        let mut outbound =
            TunnelServerOutboundStream::new(writer, outbound_crypt, channels.1, local_stop.clone());

        tokio::spawn(async move {
            if let Err(e) = inbound.run().await {
                log::error!("Inbound stream error: {:?}", e);
            }
            // let's ensure the other side is also stopped
            inbound.stop.trigger();
        });

        tokio::spawn(async move {
            if let Err(e) = outbound.run().await {
                log::error!("Outbound stream error: {:?}", e);
            }
            // let's ensure the other side is also stopped
            outbound.stop.trigger();
        });

        tokio::spawn(async move {
            // Notify starting server side
            if let Err(e) = session_manager.start_server(&session_id).await {
                log::error!("Failed to start server session {:?}: {:?}", session_id, e);
                local_stop.trigger();
                // Note: Server side does not trigger stop of the session on failure
                //       as it is recoverable.
                return;
            }
            tokio::select! {
                _ = stop.wait_async() => {
                    local_stop.trigger();
                }
                _ = local_stop.wait_async() => {}
            }
            // Notify stopping server side
            if let Err(e) = session_manager.stop_server(&session_id).await {
                log::error!("Failed to stop server session {:?}: {:?}", session_id, e);
            }

            // Insert a task that, after a couple of seconds, sets the stop trigger
            // if the session already exists and the server is not running
            tokio::spawn({
                let stop = stop.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS))
                        .await;
                    if let Some(session) = session_manager.get_session(&session_id)
                        && !session.is_server_running()
                    {
                        log::info!(
                            "Server side not running for session {:?}, setting stop trigger",
                            session_id
                        );
                        stop.trigger();
                    }
                }
            });
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
