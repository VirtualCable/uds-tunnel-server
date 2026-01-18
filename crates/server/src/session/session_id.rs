use std::sync::{Arc, RwLock, atomic::AtomicBool};

use anyhow::Result;
use flume::{Receiver, Sender};
use rand::{Rng, distr::Alphanumeric};

use super::proxy::{Proxy, SessionProxyHandle};
use crate::{consts::TICKET_LENGTH, crypt, log, system::trigger::Trigger};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; TICKET_LENGTH]);

impl SessionId {
    pub fn new() -> Self {
        let rng = rand::rng();
        let id = rng
            .sample_iter(Alphanumeric)
            .take(TICKET_LENGTH)
            .collect::<Vec<u8>>()
            .try_into()
            .expect("Failed to create SessionId");
        SessionId(id)
    }
    pub fn from(id: [u8; TICKET_LENGTH]) -> Self {
        SessionId(id)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        SessionId::new()
    }
}

impl From<[u8; TICKET_LENGTH]> for SessionId {
    fn from(id: [u8; TICKET_LENGTH]) -> Self {
        SessionId::from(id)
    }
}

pub struct Session {
    ticket: [u8; TICKET_LENGTH],
    shared_secret: [u8; 32],
    stop: Trigger,
    // Channels for server <-> client communication
    session_proxy: SessionProxyHandle,

    // Server is the side that accepted the connection from the client
    is_server_running: AtomicBool,
    // Client is the side that initiated the connection to the remote server
    is_client_running: AtomicBool,
    // seq numbers for crypto part
    // only updated on server side killed.
    seq: Arc<RwLock<(u64, u64)>>,
}

impl Session {
    pub fn new(shared_secret: [u8; 32], ticket: [u8; TICKET_LENGTH], stop: Trigger) -> Self {
        let (proxy, session_proxy) = Proxy::new(stop.clone());
        proxy.run(); // Start proxy task

        Session {
            shared_secret,
            ticket,
            session_proxy,
            stop,
            is_server_running: AtomicBool::new(false),
            is_client_running: AtomicBool::new(false),
            seq: Arc::new(RwLock::new((0, 0))),
        }
    }

    pub async fn get_server_channels(&self) -> Result<(Sender<Vec<u8>>, Receiver<Vec<u8>>)> {
        let endpoints = self.session_proxy.attach_server().await?;
        Ok((endpoints.tx, endpoints.rx))
    }

    pub async fn get_client_channels(&self) -> Result<(Sender<Vec<u8>>, Receiver<Vec<u8>>)> {
        let endpoints = self.session_proxy.attach_client().await?;
        Ok((endpoints.tx, endpoints.rx))
    }

    pub async fn start_server(&self) -> Result<()> {
        self.is_server_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub async fn stop_server(&self) -> Result<()> {
        self.is_server_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Ensure proxy detaches server channels
        self.session_proxy.detach_server().await
    }

    pub fn is_server_running(&self) -> bool {
        self.is_server_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn start_client(&self) -> Result<()> {
        self.is_client_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub async fn stop_client(&self) -> Result<()> {
        self.is_client_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub fn is_client_running(&self) -> bool {
        self.is_client_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_seq(&self, seq_tx: u64, seq_rx: u64) {
        if let Ok(mut seq_lock) = self.seq.write() {
            *seq_lock = (seq_tx, seq_rx);
        }
    }

    pub fn get_seq(&self) -> (u64, u64) {
        if let Ok(seq_lock) = self.seq.read() {
            *seq_lock
        } else {
            (0, 0)
        }
    }

    pub fn get_ticket(&self) -> &[u8; TICKET_LENGTH] {
        &self.ticket
    }

    pub fn get_shared_secret(&self) -> [u8; 32] {
        self.shared_secret
    }

    pub fn get_stop_trigger(&self) -> Trigger {
        self.stop.clone()
    }

    pub fn get_server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        crypt::tunnel::get_tunnel_crypts(&self.shared_secret, self.get_ticket())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.set();
    }
}
