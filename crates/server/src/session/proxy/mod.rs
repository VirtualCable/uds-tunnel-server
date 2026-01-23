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

    pub async fn attach_client(&self) -> Result<ClientEndpoints> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = ProxyCommand::AttachClient { reply: reply_tx };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = reply_rx.recv_async().await?;
        Ok(endpoints)
    }
}

enum ProxyCommand {
    AttachServer { reply: Sender<ServerEndpoints> },
    AttachClient { reply: Sender<ClientEndpoints> },
    DetachServer,
}

#[derive(Debug, Clone)]
pub(super) struct ServerEndpoints {
    pub tx: Sender<Vec<u8>>,
    pub rx: Receiver<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(super) struct ClientEndpoints {
    pub tx: Sender<Vec<u8>>,
    pub rx: Receiver<Vec<u8>>,
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
        let Self { ctrl_rx, stop } = self;

        // Now we need the other sides for both sides (our sides)
        let mut our_server_channels: Option<ServerEndpoints> = None;
        let mut our_client_channels: Option<ClientEndpoints> = None;

        log::debug!("Session proxy started");

        loop {
            let (server_recv, client_recv) = if let Some(chs) = &our_server_channels
                && let Some(chc) = &our_client_channels
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
                        Ok(ProxyCommand::AttachClient { reply }) => {
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
                        Err(_) => {
                            log::debug!("Control channel closed, stopping session proxy");
                            break
                        }
                    }
                }
                msg = server_recv => {
                    if let Ok(msg) = msg && let Some(client) = &our_client_channels {
                        let _ = client.tx.send_async(msg).await;
                    }
                }
                msg = client_recv => {
                    if let Ok(msg) = msg && let Some(server) = &our_server_channels {
                        let _ = server.tx.send_async(msg).await;
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
