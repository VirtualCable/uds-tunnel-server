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
use flume::{Receiver, bounded};
use futures::future::{Either, pending};

use crate::session::{SessionId, SessionManager};
use shared::{log, protocol, system::trigger::Trigger};

mod clients;
pub mod handler;
pub mod types;

pub(super) struct Proxy {
    ctrl_rx: Receiver<handler::Command>,
    stop: Trigger,
}

impl Proxy {
    pub fn new(stop: Trigger) -> (Self, handler::Handler) {
        let (ctrl_tx, ctrl_rx) = bounded(4); // Control channel, small buffer
        let proxy = Proxy { ctrl_rx, stop };
        let handle = handler::Handler::new(ctrl_tx);
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

        let mut clients = clients::ClientFanIn::new();

        // Now we need the other sides for both sides (our sides)
        let mut our_server_channels: Option<types::ServerEndpoints> = None;

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
                        Ok(handler::Command::AttachServer { reply }) => {
                            log::debug!("Attaching server to session proxy");
                            let (server_tx, our_rx) = protocol::payload_with_channel_pair();
                            let (our_tx, server_rx) = protocol::payload_with_channel_pair();
                            our_server_channels = Some(types::ServerEndpoints { tx: our_tx, rx: our_rx });
                            let endpoints = types::ServerEndpoints { tx: server_tx, rx: server_rx };
                            let _ = reply.send(endpoints);
                        }
                        Ok(handler::Command::ServerFailed) => {
                            log::debug!("Detaching server from session proxy");
                            our_server_channels = None;
                        }
                        Ok(handler::Command::ServerStopped) => {
                            log::debug!("Server stopped, closing session proxy");
                            break;  // exit loop on server stopped
                        }
                        Ok(handler::Command::ClientStopped(stream_channel_id)) => {
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
                        Ok(msg) => {
                            // stream channel 0 is control channel, process it here
                            if msg.channel_id == 0 {
                                // Failures on commands closes the proxy and consecuently, the session
                                if Self::handle_incoming_command(msg.payload.as_ref(), &parent, &mut clients).await? {
                                    log::debug!("Control channel requested session close");
                                    break;  // exit loop on command request
                                }
                                continue;
                            }
                            let channel_id = msg.channel_id;
                            if let Err(e) = clients.send_to_channel(msg).await {
                                log::warn!("Failed to forward message to client: {:?}", e);
                                // Return error to server and continue
                                if let Some(server) = &our_server_channels {
                                    let _ = server.tx.send_async(
                                        protocol::Command::ChannelError {
                                            channel_id,
                                            message: format!("Failed to forward message to client: {:?}", e)
                                        }.to_message()
                                    ).await;
                                }
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
                        Ok(msg) => {
                            if let Some(server) = &our_server_channels && let Err(e) = server.tx.send_async(msg).await {
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
        data: &[u8],
        parent: &SessionId,
        clients: &mut clients::ClientFanIn,
    ) -> Result<bool> {
        // Errors parsing commands, mean intentional error or misbehavior (or big bug :P), so we will always
        // close the session on command errors
        let cmd = protocol::Command::from_slice(data)?;
        log::debug!("Processing command in proxy: {:?}", cmd);
        match cmd {
            protocol::Command::Close => {
                log::info!(
                    "Received Close command in session {:?}, closing session",
                    parent
                );
                // Just return an error to close the session
                return Ok(true);
            }
            protocol::Command::OpenChannel { channel_id } => {
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
            protocol::Command::CloseChannel { channel_id } => {
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
