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

use std::{
    net::SocketAddr,
    sync::{RwLock, atomic::AtomicBool},
};

use anyhow::Result;
use flume::{Receiver, Sender};

use shared::{crypt, crypt::types::SharedSecret, log, system::trigger::Trigger, ticket};

mod manager;
mod proxy;

pub use manager::SessionManager;

// Alias, internal SessionId is a Ticket
pub type SessionId = ticket::Ticket;

pub struct Session {
    ticket: ticket::Ticket,
    stream_channel_id: u16,
    shared_secret: SharedSecret,
    stop: Trigger,
    // Channels for server <-> client communication
    session_proxy: proxy::SessionProxyHandle,

    // Server is the side that accepted the connection from the client
    is_server_running: AtomicBool,
    // Client is the side that initiated the connection to the remote server
    is_client_running: AtomicBool,

    // seq numbers for crypto part
    // only updated on server side killed. (the one receives/sends data from client)
    seq: RwLock<(u64, u64)>,

    // Ip of the client connected
    ip: RwLock<SocketAddr>,
}

impl Session {
    pub fn new(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stream_channel_id: u16,
        stop: Trigger,
        ip: SocketAddr,
    ) -> Self {
        let (proxy, session_proxy) = proxy::Proxy::new(stop.clone());
        proxy.run(); // Start proxy task

        Session {
            ticket,
            stream_channel_id,
            shared_secret,
            stop,
            session_proxy,
            is_server_running: AtomicBool::new(false),
            is_client_running: AtomicBool::new(false),
            seq: RwLock::new((0, 0)),
            ip: RwLock::new(ip),
        }
    }

    pub fn set_ip(&self, ip: SocketAddr) {
        if let Ok(mut ip_lock) = self.ip.write() {
            *ip_lock = ip;
        }
    }

    pub async fn server_sender_receiver(&self) -> Result<(Sender<Vec<u8>>, Receiver<Vec<u8>>)> {
        let endpoints = self.session_proxy.attach_server().await?;
        Ok((endpoints.tx, endpoints.rx))
    }

    pub async fn client_sender_receiver(&self) -> Result<(Sender<Vec<u8>>, Receiver<Vec<u8>>)> {
        let endpoints = self.session_proxy.attach_client().await?;
        Ok((endpoints.tx, endpoints.rx))
    }

    pub async fn stop(&self) {
        log::info!("Stopping session");
        self.stop.trigger();
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

    pub fn set_inbound_seq(&self, seq_rx: u64) {
        if let Ok(mut seq_lock) = self.seq.write() {
            seq_lock.0 = seq_rx;
        }
    }

    pub fn set_outbound_seq(&self, seq_tx: u64) {
        if let Ok(mut seq_lock) = self.seq.write() {
            seq_lock.1 = seq_tx;
        }
    }

    // Returns the (inbound, outbound) seq numbers
    pub fn seqs(&self) -> (u64, u64) {
        if let Ok(seq_lock) = self.seq.read() {
            *seq_lock
        } else {
            (0, 0)
        }
    }

    pub fn ticket(&self) -> &ticket::Ticket {
        &self.ticket
    }

    pub fn shared_secret(&self) -> &SharedSecret {
        &self.shared_secret
    }

    pub fn stop_trigger(&self) -> Trigger {
        self.stop.clone()
    }

    pub fn server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        crypt::tunnel::get_tunnel_crypts(&self.shared_secret, self.ticket(), self.seqs())
    }

    pub fn stream_channel_id(&self) -> u16 {
        self.stream_channel_id
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.trigger();
    }
}
