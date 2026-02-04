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
use flume::{Receiver, Sender, bounded};
use futures::future::{Either, pending};
use tokio::net::TcpStream;

use crate::{
    consts::CHANNEL_SIZE,
    session::{Session, SessionId, SessionManager},
    stream::client::TunnelClientStream,
};
use shared::{log, protocol::command::Command, system::trigger::Trigger};

enum ProxyCommand {
    AttachServer { reply: Sender<ServerEndpoints> },
    ServerFailed,  // Will not close the proxy, to allow recovery
    ServerStopped, // Will close the proxy, as the server is done
    // Client is attached by us, so no need for an attach command
    ClientStopped(u16), // stream_channel_id, no need to know if it failed or stopped normally
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
    pub async fn start_server(&self) -> Result<ServerEndpoints> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = ProxyCommand::AttachServer { reply: reply_tx };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = reply_rx.recv_async().await?;
        Ok(endpoints)
    }

    pub async fn stop_server(&self) {
        if let Err(e) = self.ctrl_tx.send_async(ProxyCommand::ServerStopped).await {
            log::error!(
                "Failed to send stop server command to session proxy: {:?}",
                e
            );
        }
    }

    pub async fn fail_server(&self) {
        if let Err(e) = self.ctrl_tx.send_async(ProxyCommand::ServerFailed).await {
            log::error!(
                "Failed to send fail server command to session proxy: {:?}",
                e
            );
        }
    }

    pub async fn stop_client(&self, stream_channel_id: u16) {
        if let Err(e) = self
            .ctrl_tx
            .send_async(ProxyCommand::ClientStopped(stream_channel_id))
            .await
        {
            log::error!(
                "Failed to send stop client command to session proxy: {:?}",
                e
            );
        }
    }
}

#[derive(Debug, Clone)]
struct ClientInfo {
    sender: Sender<Vec<u8>>,
    stop: Trigger,
}

struct ClientFanIn {
    clients_senders: Vec<Option<ClientInfo>>,
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

    async fn create_client(&mut self, stream_channel_id: u16, session: Arc<Session>) -> Result<()> {
        // Ensure vector is large enough
        if self.clients_senders.len() < stream_channel_id as usize {
            self.clients_senders
                .resize(stream_channel_id as usize, None);
        }

        // If current client is Some, we are replacing it, so ensure old one receives the stop signal
        if let Some(old_client) = &self.clients_senders[(stream_channel_id - 1) as usize] {
            // Ensure notify old client to stop before replacing
            old_client.stop.trigger();
        }

        let (sender, receiver) = flume::bounded(CHANNEL_SIZE);
        // (self.sender.clone(), receiver)

        // If outside remotes, will fail and return error
        let target_stream =
            TcpStream::connect(&session.remotes[stream_channel_id as usize - 1]).await?;

        // Split the target stream into reader and writer
        let (target_reader, target_writer) = target_stream.into_split();

        let stop = Trigger::new();

        // Note: The TunnelClientStream will not receive the global stop, but its own stop trigger
        // managed by the ClientFanIn
        let client_stream = TunnelClientStream::new(
            *session.id(),
            stop.clone(),
            stream_channel_id,
            target_reader,
            target_writer,
            ClientEndpoints {
                tx: self.sender.clone(),
                rx: receiver,
            },
        );

        // Spawn a task to run the client stream
        tokio::spawn(async move {
            if let Err(e) = client_stream.run().await {
                log::error!("Client stream error: {:?}", e);
            }
        });

        self.clients_senders[(stream_channel_id - 1) as usize] = Some(ClientInfo { sender, stop });
        Ok(())
    }

    pub async fn send_to_channel(&self, stream_channel_id: u16, msg: Vec<u8>) -> Result<()> {
        if stream_channel_id == 0 || stream_channel_id as usize > self.clients_senders.len() {
            return Err(anyhow::anyhow!(
                "Invalid stream_channel_id: {}",
                stream_channel_id
            ));
        }
        if let Some(client) = &self.clients_senders[(stream_channel_id - 1) as usize] {
            client.sender.send_async(msg).await?;
        }
        // If no client, just drop the message
        Ok(())
    }

    pub async fn stop_client(&self, stream_channel_id: u16) {
        if stream_channel_id == 0 || stream_channel_id as usize > self.clients_senders.len() {
            return;
        }
        if let Some(client) = &self.clients_senders[(stream_channel_id - 1) as usize] {
            client.stop.trigger();
        }
    }

    pub fn stop_all_clients(&self) {
        for client in self.clients_senders.iter().flatten() {
            client.stop.trigger();
        }
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
                if let Err(e) = self.run_session_proxy(parent).await {
                    log::error!("Session proxy encountered an error: {:?}", e);
                } else {
                    log::debug!("Session proxy exited normally");
                }
                // Exiting proxy means end of session, as there is no possible recovery
                stop.trigger();
                // Remove session from manager, as it is ended
                let session_manager = crate::session::manager::SessionManager::get_instance();
                log::debug!("Removing session {:?} from {:?}", parent, session_manager);
                session_manager.remove_session(&parent);
            }
        })
    }

    async fn run_session_proxy(self, parent: SessionId) -> Result<()> {
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
                        Ok(ProxyCommand::ServerFailed) => {
                            log::debug!("Detaching server from session proxy");
                            our_server_channels = None;
                        }
                        Ok(ProxyCommand::ServerStopped) => {
                            log::debug!("Server stopped, closing session proxy");
                            break;  // exit loop on server stopped
                        }
                        Ok(ProxyCommand::ClientStopped(stream_channel_id)) => {
                            log::debug!("Client {} stopped, removing from session proxy", stream_channel_id);
                            clients.stop_client(stream_channel_id).await;
                            clients.close_client(stream_channel_id);
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
                            // stream channel 0 is control channel, process it here
                            if stream_channel_id == 0 {
                                // Failures on commands closes the proxy and consecuently, the session
                                if Self::handle_incoming_command(msg, &parent, &mut clients).await? {
                                    log::debug!("Control channel requested session close");
                                    break;  // exit loop on command request
                                }
                                continue;
                            }
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
        log::debug!("Session proxy exiting, cleaning up clients");
        // Stop all clients. Do not need to clean up the clients vector, as we are exiting anyway
        clients.stop_all_clients();
        Ok(())
    }

    async fn handle_incoming_command(
        data: Vec<u8>,
        parent: &SessionId,
        clients: &mut ClientFanIn,
    ) -> Result<bool> {
        // Errors parsing commands, mean intentional error or misbehavior (or big bug :P), so we will always
        // close the session on command errors
        let cmd = Command::from_bytes(&data)?;
        log::debug!("Processing command in proxy: {:?}", cmd);
        match cmd {
            Command::Close => {
                log::info!(
                    "Received Close command in session {:?}, closing session",
                    parent
                );
                // Just return an error to close the session
                return Ok(true);
            }
            Command::OpenChannel { channel_id } => {
                let session = {
                    let session_manager = SessionManager::get_instance();
                    session_manager
                        .get_session(parent)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Session {:?} not found when opening channel", parent)
                        })?
                        .clone()
                };
                clients.create_client(channel_id, session).await?;
            }
            Command::CloseChannel { channel_id } => {
                clients.stop_client(channel_id).await;
                clients.close_client(channel_id);
            }
            _ => {
                log::warn!(
                    "Received unexpected command in session {:?}: {:?}",
                    parent,
                    cmd
                );
                // Other commands are unexpected on control channel, log them and close session
                return Err(anyhow::anyhow!(
                    "Unexpected command on control channel: {:?}",
                    cmd
                ));
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests;
