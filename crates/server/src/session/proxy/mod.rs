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
use flume::{Receiver, Sender, bounded};
use futures::future::{Either, pending};

use crate::consts::CHANNEL_SIZE;
use shared::{log, system::trigger::Trigger};

enum ProxyCommand {
    AttachServer {
        reply: Sender<ServerEndpoints>,
    },
    AttachClient {
        channel_id: u16,
        reply: Sender<ClientEndpoints>,
    },
    DetachServer,
    DetachClient {
        channel_id: u16,
    },
}

#[derive(Debug, Clone)]
pub struct ServerEndpoints {
    pub tx: Sender<(u16, Vec<u8>)>,
    pub rx: Receiver<(u16, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub struct ClientEndpoints {
    pub tx: Sender<Vec<u8>>,
    pub rx: Receiver<Vec<u8>>,
}

pub(super) struct SessionProxyHandle {
    ctrl_tx: Sender<ProxyCommand>,
}

impl SessionProxyHandle {
    pub async fn attach_server(&self) -> Result<ServerEndpoints> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = ProxyCommand::AttachServer { reply: reply_tx };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = reply_rx.recv_async().await?;
        Ok(endpoints)
    }

    pub async fn detach_server(&self) -> Result<()> {
        let cmd = ProxyCommand::DetachServer;
        self.ctrl_tx.send_async(cmd).await?;
        Ok(())
    }

    pub async fn attach_client(&self, channel_id: u16) -> Result<ClientEndpoints> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = ProxyCommand::AttachClient {
            channel_id,
            reply: reply_tx,
        };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = reply_rx.recv_async().await?;
        Ok(endpoints)
    }

    pub async fn detach_client(&self, channel_id: u16) -> Result<()> {
        let cmd = ProxyCommand::DetachClient { channel_id };
        self.ctrl_tx.send_async(cmd).await?;
        Ok(())
    }
}

pub(super) struct Proxy {
    ctrl_rx: Receiver<ProxyCommand>,
    channel_ids: Vec<u16>,
    stop: Trigger,
}

impl Proxy {
    pub fn new(channel_ids: &[u16], stop: Trigger) -> (Self, SessionProxyHandle) {
        let (ctrl_tx, ctrl_rx) = bounded(4); // Control channel, small buffer
        let proxy = Proxy {
            ctrl_rx,
            channel_ids: channel_ids.to_vec(),
            stop,
        };
        let handle = SessionProxyHandle { ctrl_tx };
        (proxy, handle)
    }

    pub fn run(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // Catch panics to avoid bringing down the server
            if let Err(e) = self.run_session_proxy().await {
                log::error!("Session proxy encountered an error: {:?}", e);
            } else {
                log::debug!("Session proxy exited normally");
            }
        })
    }

    async fn run_session_proxy(self) -> Result<()> {
        let Self {
            ctrl_rx,
            stop,
            channel_ids: channels_ids,
        } = self;

        if channels_ids.len() != 1 {
            log::warn!(
                "Session proxy started with invalid channel IDs length: {}, expected 1",
                channels_ids.len()
            );
            return Ok(());
        }

        let stream_channel_id = channels_ids[0];

        // TODO:
        // We will hold an array of client channels in the future, indexed by channel id
        // All channels id, starting at one, must be sequential, to make a simple array possible
        // The idea is receive a message from server with channel id, and forward to the correct client channel
        // And viceversa, receive from client channel, and forward to server with the correct channel
        // To allow simple multiplexing of multiple channels over a single session proxy, we will have
        // a MPSC channel (for client to proxy) for all clients, and a SPSC channel (for proxy to client) for each client

        // Now we need the other sides for both sides (our sides)
        let mut our_server_channels: Option<ServerEndpoints> = None;
        let mut our_client_channels: Option<ClientEndpoints> = None;

        log::debug!("Session proxy started");

        loop {
            // We can have already the channels, but they can be disconnected
            // Any disconnected channel is considered as non-existing
            // else, this loop will busy-wait, and hold tokio
            let (server_recv, client_recv) = if let Some(chs) = &our_server_channels
                && let Some(chc) = &our_client_channels
                && !chs.rx.is_disconnected()
                && !chs.tx.is_disconnected()
                && !chc.rx.is_disconnected()
                && !chc.tx.is_disconnected()
            {
                (
                    Either::Left(chs.rx.recv_async()),
                    Either::Left(chc.rx.recv_async()),
                )
            } else {
                (Either::Right(pending()), Either::Right(pending()))
            };

            tokio::select! {
                _ = stop.wait_async() => {
                    log::debug!("Session proxy stopping due to stop signal");
                    break;
                }

                cmd = ctrl_rx.recv_async() => {
                    match cmd {
                        Ok(ProxyCommand::AttachServer { reply }) => {
                            log::debug!("Attaching server to session proxy");
                            let (server_tx, our_rx) = bounded(CHANNEL_SIZE);
                            let (our_tx, server_rx) = bounded(CHANNEL_SIZE);
                            our_server_channels = Some(ServerEndpoints { tx: our_tx, rx: our_rx });
                            let endpoints = ServerEndpoints { tx: server_tx, rx: server_rx };
                            let _ = reply.send(endpoints);
                        }
                        Ok(ProxyCommand::AttachClient { channel_id, reply }) => {
                            // Note: currently we only support one channel id per proxy
                            if channel_id != stream_channel_id {
                                log::error!("Attempt to attach client with invalid channel id: {}, expected: {}", channel_id, stream_channel_id);
                                break;  // exit loop on error
                            }
                            log::debug!("Attaching client to session proxy");
                            let (client_tx, our_rx) = bounded(CHANNEL_SIZE);
                            let (our_tx, client_rx) = bounded(CHANNEL_SIZE);
                            our_client_channels = Some(ClientEndpoints { tx: our_tx, rx: our_rx });
                            let endpoints = ClientEndpoints { tx: client_tx, rx: client_rx };
                            let _ = reply.send(endpoints);
                        }
                        Ok(ProxyCommand::DetachServer) => {
                            log::debug!("Detaching server from session proxy");
                            our_server_channels = None;
                        }
                        Ok(ProxyCommand::DetachClient { channel_id }) => {
                            if channel_id != stream_channel_id {
                                log::error!("Attempt to detach client with invalid channel id: {}, expected: {}", channel_id, stream_channel_id);
                                break;  // exit loop on error
                            }
                            log::debug!("Detaching client from session proxy");
                            // Now, just exit the loop. On future, when all channels are detached, we will exit the loop
                            break;
                        }
                        Err(_) => {
                            log::debug!("Control channel closed, stopping session proxy");
                            break
                        }
                    }
                }
                msg = server_recv => {
                    match msg {
                        Ok((channel_id, msg)) => {
                            // TODO:  Future Stream channel id will made to send to different client channels
                            if stream_channel_id != channel_id {
                                log::warn!("Received message for unknown stream channel id: {}, expected: {}", stream_channel_id, channel_id);
                                break;  // exit loop on error
                            }
                            if let Some(client) = &our_client_channels && let Err(e) = client.tx.send_async(msg).await {
                                log::warn!("Failed to forward message to client: {:?}", e);
                                break;  // exit loop on error
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to receive message from server: {:?}", e);
                            break;  // exit loop on error
                        }
                    }
                }
                msg = client_recv => {
                    match msg {
                        Ok(msg) => {
                            if let Some(server) = &our_server_channels && let Err(e) = server.tx.send_async((stream_channel_id, msg)).await {
                                log::warn!("Failed to forward message to server: {:?}", e);
                                break;  // exit loop on error
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to receive message from client: {:?}", e);
                            break;  // exit loop on error
                        }
                    }
                }
            }
        }
        log::info!("Session proxy stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests;
