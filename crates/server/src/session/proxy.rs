use anyhow::Result;
use flume::{Receiver, Sender, bounded};
use futures::future::{Either, pending};

use crate::{consts::CHANNEL_SIZE, system::trigger::Trigger};

pub(super) struct SessionProxyHandle {
    ctrl_tx: Sender<ProxyCommand>,
}

impl SessionProxyHandle {
    pub(super) async fn attach_server(&self) -> Result<ServerEndpoints> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = ProxyCommand::AttachServer { reply: reply_tx };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = reply_rx.recv_async().await?;
        Ok(endpoints)
    }

    pub(super) async fn attach_client(&self) -> Result<ClientEndpoints> {
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
    client_channels: Option<ClientEndpoints>,
    server_channels: Option<ServerEndpoints>,
    ctrl_rx: Receiver<ProxyCommand>,
    stop: Trigger,
}

impl Proxy {
    pub fn new(stop: Trigger) -> (Self, SessionProxyHandle) {
        let (ctrl_tx, ctrl_rx) = bounded(4); // Control channel, small buffer
        let proxy = Proxy {
            client_channels: None,
            server_channels: None,
            ctrl_rx,
            stop,
        };
        let handle = SessionProxyHandle { ctrl_tx };
        (proxy, handle)
    }

    pub fn run(self) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move { self.run_session_proxy().await })
    }

    async fn run_session_proxy(self) -> Result<()> {
        let Self {
            mut client_channels,
            mut server_channels,
            ctrl_rx,
            stop,
        } = self; // Consume self

        loop {
            let server_recv = match &server_channels {
                Some(ch) => Either::Left(ch.rx.recv_async()),
                None => Either::Right(pending()),
            };
            let client_recv = match &client_channels {
                Some(ch) => Either::Left(ch.rx.recv_async()),
                None => Either::Right(pending()),
            };
            tokio::select! {
                _ = stop.async_wait() => {
                    break;
                }
                cmd = ctrl_rx.recv_async() => {
                    match cmd {
                        Ok(ProxyCommand::AttachServer { reply }) => {
                            let (tx, rx) = bounded(CHANNEL_SIZE);
                            let endpoints = ServerEndpoints { tx, rx };
                            server_channels = Some(endpoints.clone());
                            let _ = reply.send(endpoints);
                        }
                        Ok(ProxyCommand::AttachClient { reply }) => {
                            let (tx, rx) = bounded(CHANNEL_SIZE);
                            let endpoints = ClientEndpoints { tx, rx };
                            client_channels = Some(endpoints.clone());
                            let _ = reply.send(endpoints);
                        }
                        Err(_) => {
                            break;
                        }
                    }
                }
                msg = server_recv => {
                    if let Ok(msg) = msg && let Some(client) = &client_channels {
                        let _ = client.tx.send_async(msg).await;
                    }
                }
                msg = client_recv => {
                    if let Ok(msg) = msg && let Some(server) = &server_channels {
                        let _ = server.tx.send_async(msg).await;
                    }
                }
            }
        }
        Ok(())
    }
}
