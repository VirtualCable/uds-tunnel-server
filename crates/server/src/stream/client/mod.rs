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

use crate::session::{ClientEndpoints, SessionId, SessionManager};

use shared::{crypt::consts::CRYPT_PACKET_SIZE, log, protocol::{PayloadWithChannel, PayloadWithChannelSender, Command, PayloadReceiver}, system::trigger::Trigger};

struct TunnelClientInboundStream<R: AsyncReadExt + Unpin> {
    stream_channel_id: u16,
    stop: Trigger,
    sender: PayloadWithChannelSender,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelClientInboundStream<R> {
    pub fn new(
        stream_channel_id: u16,
        reader: R,
        sender: PayloadWithChannelSender,
        stop: Trigger,
    ) -> Self {
        TunnelClientInboundStream {
            stream_channel_id,
            stop,
            sender,
            reader,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting client inbound stream");
        // Read from read_half, raw, decrypt and send to sender channel, raw
        let mut buffer = [0u8; CRYPT_PACKET_SIZE];
        loop {
            tokio::select! {
                _ = self.stop.wait_async() => {
                    log::debug!("Stopping client inbound stream due to stop signal");
                    break;
                }
                result = self.reader.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            log::debug!("Client inbound stream reached EOF");
                            // Connection closed, send message
                            self.sender.try_send(
                                Command::CloseChannel {
                                    channel_id: self.stream_channel_id
                                }.to_message()
                            )?;
                            break;
                        }
                        Ok(count) => {
                            // Send to channel, fail if full or disconnected
                            // Does not wait for space in channel
                            // This is an internal error, and there is no way to send error here.
                            self.sender.try_send(PayloadWithChannel::new(self.stream_channel_id, &buffer[..count]))?;
                        }
                        Err(e) => {
                            log::error!("Client inbound read error: {:?}", e);
                            self.sender.try_send(
                                Command::ChannelError {
                                    channel_id: self.stream_channel_id,
                                    message: format!("Client read error: {:?}", e),
                                }.to_message()
                            )?;
                            return Err(anyhow::anyhow!("Client inbound read error: {:?}", e));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

struct TunnelClientOutboundStream<W: AsyncWriteExt + Unpin> {
    stop: Trigger,
    err_sender: PayloadWithChannelSender, // Just for error reporting, not used for normal data
    receiver: PayloadReceiver,

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelClientOutboundStream<W> {
    pub fn new(
        writer: W,
        err_sender: PayloadWithChannelSender,
        receiver: PayloadReceiver,
        stop: Trigger,
    ) -> Self {
        TunnelClientOutboundStream {
            stop,
            err_sender,
            receiver,
            writer,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        // Run on client side is mandatory. If run ends, stop must be set. in any case.
        log::debug!("Starting client outbound stream");
        loop {
            tokio::select! {
                _ = self.stop.wait_async() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(data) => {
                            match self.writer.write_all(&data.0).await {
                                Ok(_) => {}
                                Err(e) => {
                                    log::error!("Client outbound write error: {:?}", e);
                                    self.err_sender.try_send(
                                        Command::ChannelError {
                                            channel_id: 0, // No channel id, this is a connection error
                                            message: format!("Client write error: {:?}", e),
                                        }.to_message()
                                    )?;
                                    return Err(anyhow::anyhow!("Client outbound write error: {:?}", e));
                                }
                            }
                        }
                        Err(_) => {
                            // Maybe the receiver "won" the select! but stop is already set. This is fine
                            // Note: internal channel error, not a client error, so we just log and return
                            if self.stop.is_triggered() {
                                break;
                            }
                            log::error!("Client outbound receiver channel closed");
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct TunnelClientStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    session_id: SessionId,
    local_stop: Trigger,
    stream_channel_id: u16,
    reader: R,
    writer: W,
    channels: ClientEndpoints,
}

impl<R, W> TunnelClientStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    pub fn new(
        session_id: SessionId,
        local_stop: Trigger,
        stream_channel_id: u16,
        reader: R,
        writer: W,
        channels: ClientEndpoints,
    ) -> Self {
        TunnelClientStream {
            session_id,
            local_stop,
            stream_channel_id,
            reader,
            writer,
            channels,
        }
    }

    pub async fn run(self) -> Result<()> {
        let Self {
            session_id,
            local_stop,
            stream_channel_id,
            reader,
            writer,
            channels,
        } = self;

        let session_manager = SessionManager::get_instance();

        let stop = if let Some(session) = session_manager.get_session(&session_id) {
            session.stop_trigger()
        } else {
            log::warn!("Session {:?} not found, aborting stream", session_id);
            return Err(anyhow::anyhow!(
                "Session {:?} not found, aborting stream",
                session_id
            ));
        };

        let error_sender = channels.tx.clone();
        let mut inbound = TunnelClientInboundStream::new(
            stream_channel_id,
            reader,
            channels.tx,
            local_stop.clone(),
        );

        let mut outbound =
            TunnelClientOutboundStream::new(writer, error_sender, channels.rx, local_stop.clone());
        tokio::spawn(async move {
            if let Err(e) = inbound.run().await {
                log::error!("Client inbound stream error: {:?}", e);
            }
            inbound.stop.trigger();
        });
        tokio::spawn(async move {
            if let Err(e) = outbound.run().await {
                log::error!("Client outbound stream error: {:?}", e);
            }
            outbound.stop.trigger();
        });
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.wait_async() => {
                    local_stop.trigger();
                }
                _ = local_stop.wait_async() => {
                }
            }
            log::debug!("Client stream for session {:?} stopping", session_id);
            // Notify stopping client side
            session_manager
                .stop_client(&session_id, stream_channel_id)
                .await;
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
