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

use crate::{consts::CHANNEL_SIZE, session::SessionId};
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
    pub tx: Sender<(u16, Vec<u8>)>,
    pub rx: Receiver<Vec<u8>>,
}

#[derive(Debug)]
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

struct ClientFanIn {
    clients_senders: Vec<Option<Sender<Vec<u8>>>>,
    sender: Sender<(u16, Vec<u8>)>,
    receiver: Receiver<(u16, Vec<u8>)>,
}

impl ClientFanIn {
    pub fn new() -> Self {
        let (sender, receiver) = flume::bounded(CHANNEL_SIZE);
        Self {
            clients_senders: Vec::new(),
            sender,
            receiver,
        }
    }

    pub fn add_client(&mut self, stream_channel_id: u16, client: Sender<Vec<u8>>) {
        if self.clients_senders.len() < stream_channel_id as usize {
            self.clients_senders
                .resize(stream_channel_id as usize, None);
        }
        self.clients_senders[(stream_channel_id - 1) as usize] = Some(client);
    }

    #[allow(clippy::type_complexity)]
    pub fn new_channel_for(
        &mut self,
        stream_channel_id: u16,
    ) -> (Sender<(u16, Vec<u8>)>, Receiver<Vec<u8>>) {
        let (sender, receiver) = flume::bounded(CHANNEL_SIZE);
        self.add_client(stream_channel_id, sender);
        (self.sender.clone(), receiver)
    }

    pub async fn send_to_channel(&self, stream_channel_id: u16, msg: Vec<u8>) -> Result<()> {
        if stream_channel_id == 0 || stream_channel_id as usize > self.clients_senders.len() {
            return Err(anyhow::anyhow!(
                "Invalid stream_channel_id: {}",
                stream_channel_id
            ));
        }
        if let Some(client) = &self.clients_senders[(stream_channel_id - 1) as usize] {
            client.send_async(msg).await?;
        }
        // If no client, just drop the message
        Ok(())
    }

    pub async fn recv(&self) -> Result<(u16, Vec<u8>)> {
        let msg = self.receiver.recv_async().await?;
        Ok(msg)
    }

    /// Closes the client for the given stream_channel_id
    pub fn close_client(&mut self, stream_channel_id: u16) {
        if self.clients_senders.len() >= stream_channel_id as usize {
            self.clients_senders[(stream_channel_id - 1) as usize] = None;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clients_senders
            .iter()
            .all(|sender_opt| sender_opt.is_none())
    }
}

pub(super) struct Proxy {
    ctrl_rx: Receiver<ProxyCommand>,
    stop: Trigger,
}

impl Proxy {
    pub fn new(stop: Trigger) -> (Self, SessionProxyHandle) {
        let (ctrl_tx, ctrl_rx) = bounded(4); // Control channel, small buffer
        let proxy = Proxy { ctrl_rx, stop };
        let handle = SessionProxyHandle { ctrl_tx };
        (proxy, handle)
    }

    pub fn run(self, parent: SessionId) -> tokio::task::JoinHandle<()> {
        tokio::spawn({
            let stop = self.stop.clone();
            async move {
                // Catch panics to avoid bringing down the server
                if let Err(e) = self.run_session_proxy().await {
                    log::error!("Session proxy encountered an error: {:?}", e);
                } else {
                    log::debug!("Session proxy exited normally");
                }
                // Exiting proxy means end of session, as there is no possible recovery
                stop.trigger();
                let session_manager = crate::session::manager::SessionManager::get_instance();
                log::debug!("Removing session {:?} from {:?}", parent, session_manager);
                session_manager.remove_session(&parent);
            }
        })
    }

    async fn run_session_proxy(self) -> Result<()> {
        let Self { ctrl_rx, stop } = self;

        let mut clients: ClientFanIn = ClientFanIn::new();

        // Now we need the other sides for both sides (our sides)
        let mut our_server_channels: Option<ServerEndpoints> = None;

        log::debug!("Session proxy started");

        loop {
            // Disconnected server channels are treated as no server connected
            // Because we can disconnect before unataching the server.
            // The clients (the parts that connect to the remote server)
            // Have a common channel, that persists until end of proxy
            let server_recv = if let Some(chs) = &our_server_channels
                && !chs.rx.is_disconnected()
                && !chs.tx.is_disconnected()
            {
                Either::Left(chs.rx.recv_async())
            } else {
                Either::Right(pending())
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
                            log::debug!("Attaching client to session proxy");
                            let (sender, receiver) = clients.new_channel_for(channel_id);
                            let endpoints = ClientEndpoints { tx: sender, rx: receiver };
                            let _ = reply.send(endpoints);
                        }
                        Ok(ProxyCommand::DetachServer) => {
                            log::debug!("Detaching server from session proxy");
                            our_server_channels = None;
                            // If no clients here, stop the proxy as
                            // this is not recoverable
                            if clients.is_empty() {
                                log::debug!("No more clients, stopping session proxy");
                                break;
                            }
                        }
                        Ok(ProxyCommand::DetachClient { channel_id }) => {
                            log::debug!("Detaching client {} from session proxy", channel_id);
                            clients.close_client(channel_id);
                            // With no more clients, session will trigger stop
                            // No need to do anything else here
                        }
                        Err(_) => {
                            log::debug!("Control channel closed, stopping session proxy");
                            break
                        }
                    }
                }
                msg = server_recv => {
                    match msg {
                        Ok((stream_channel_id, msg)) => {
                            if let Err(e) = clients.send_to_channel(stream_channel_id, msg).await {
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
                msg = clients.recv() => {
                    match msg {
                        Ok((stream_channel_id, msg)) => {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests;
