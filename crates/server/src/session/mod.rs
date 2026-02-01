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

use std::{
    net::SocketAddr,
    sync::{
        RwLock,
        atomic::{AtomicBool, AtomicU16},
    },
};

use anyhow::Result;

use shared::{crypt, crypt::types::SharedSecret, log, system::trigger::Trigger, ticket};

mod manager;
mod proxy;

pub use {
    manager::SessionManager,
    proxy::{ClientEndpoints, ServerEndpoints},
};

// Alias, internal SessionId is a Ticket
pub type SessionId = ticket::Ticket;

#[derive(Debug)]
pub struct Session {
    id: SessionId,
    ticket: ticket::Ticket,
    shared_secret: SharedSecret,
    stop: Trigger,
    // Channels for server <-> client communication
    session_proxy: proxy::SessionProxyHandle,

    // proxy async task handle
    proxy_task: tokio::task::JoinHandle<()>,
    // Client not started will be allowed for 2 seconds
    // Client stopped, will stop session also
    // Server disconnect, allows some time before stopping session
    // to allow client reconnection
    server_running: AtomicBool,
    clients_count: AtomicU16, // So we can stop as soon as no clients are connected also

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
        stop: Trigger,
        ip: SocketAddr,
    ) -> Self {
        let (proxy, session_proxy) = proxy::Proxy::new(stop.clone());
        let id = SessionId::new_random();

        let proxy_task = proxy.run(id); // Start proxy task

        Session {
            id,
            ticket,
            shared_secret,
            stop,
            session_proxy,
            proxy_task,
            server_running: AtomicBool::new(false),
            clients_count: AtomicU16::new(0),
            seq: RwLock::new((0, 0)),
            ip: RwLock::new(ip),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn set_ip(&self, ip: SocketAddr) {
        if let Ok(mut ip_lock) = self.ip.write() {
            *ip_lock = ip;
        }
    }

    pub async fn server_sender_receiver(&self) -> Result<ServerEndpoints> {
        self.session_proxy.attach_server().await
    }

    pub async fn client_sender_receiver(&self, stream_channel_id: u16) -> Result<ClientEndpoints> {
        self.session_proxy.attach_client(stream_channel_id).await
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

    pub fn is_running(&self) -> bool {
        !self.proxy_task.is_finished()
    }

    pub fn is_server_running(&self) -> bool {
        self.is_running()
            && self
                .server_running
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_client_running(&self) -> bool {
        self.is_running() && self.clients_count.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    pub fn server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        crypt::tunnel::get_tunnel_crypts(&self.shared_secret, self.ticket(), self.seqs())
    }

    pub(super) async fn start_client(&self) -> Result<()> {
        self.clients_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Stops a client stream, returns the number of remaining running clients
    /// Note: This function will invoke stop trigger if no more clients are running
    pub(super) async fn stop_client(&self, stream_channel_id: u16) -> Result<()> {
        // Ensure proxy detaches client channels
        self.clients_count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.session_proxy.detach_client(stream_channel_id).await?;
        Ok(())
    }

    pub(super) async fn start_server(&self) -> Result<()> {
        self.server_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    pub(super) async fn stop_server(&self) -> Result<()> {
        // Ensure proxy detaches server channels
        self.server_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if !self.is_client_running() {
            log::warn!(
                "Session proxy task already finished for session {:?}",
                self.id()
            );
            Ok(())
        } else {
            self.session_proxy.detach_server().await
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.trigger();
    }
}
