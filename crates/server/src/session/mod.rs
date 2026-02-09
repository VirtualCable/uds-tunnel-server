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
    sync::{RwLock, atomic::AtomicBool},
};

use anyhow::Result;

use shared::{
    crypt::{self, types::SharedSecret},
    log,
    protocol::{
        PayloadWithChannel, PayloadWithChannelReceiver, PayloadWithChannelSender,
        payload_with_channel_pair, ticket,
    },
    system::trigger::Trigger,
};

mod manager;
mod proxy;

pub use {
    manager::SessionManager,
    proxy::types::{ClientEndpoints, ServerEndpoints},
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
    session_proxy: proxy::handler::Handler,

    // proxy async task handle
    proxy_task: tokio::task::JoinHandle<()>,
    // Server side status
    server_running: AtomicBool,

    // Session is closed when:
    //   - client (connetecto to ou server side) disconnects correctly
    //   - client sends a Close command
    //   - client does not reconnect on recovery window
    remotes: Vec<String>, // List of remote addresses that can be used on this session

    // If there is an unsent message on server side
    // (eg: client sent a message but an error ocurrend, and it's alreade consumed from channel)
    unsent_message: RwLock<Option<PayloadWithChannel>>,

    // The channels for server side must be kept in the session, as they can contain unprocessed messages
    tx: PayloadWithChannelSender,
    rx_server: PayloadWithChannelReceiver,
    tx_server: PayloadWithChannelSender,
    rx: PayloadWithChannelReceiver,

    // seq numbers for crypto part
    // only updated on server side killed. (the one receives/sends data from client)
    seq: RwLock<(u64, u64)>,

    // Ip of the client connected
    src_ip: RwLock<SocketAddr>,
}

impl Session {
    pub fn new(
        shared_secret: SharedSecret,
        ticket: ticket::Ticket,
        stop: Trigger,
        src_ip: SocketAddr,
        remotes: Vec<String>, // List of remote addresses that can be used on this session
    ) -> Self {
        let (proxy, session_proxy) = proxy::Proxy::new(stop.clone());
        let id = SessionId::new_random();

        let proxy_task = proxy.run(id); // Start proxy task

        let (tx, rx_server) = payload_with_channel_pair();
        let (tx_server, rx) = payload_with_channel_pair();

        Session {
            id,
            ticket,
            shared_secret,
            stop,
            session_proxy,
            proxy_task,
            server_running: AtomicBool::new(false),
            unsent_message: RwLock::new(None),
            tx,
            rx_server,
            tx_server,
            rx,
            seq: RwLock::new((0, 0)),
            src_ip: RwLock::new(src_ip),
            remotes,
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn take_unsent_packet(&self) -> Option<PayloadWithChannel> {
        if let Ok(mut unsent_lock) = self.unsent_message.write() {
            unsent_lock.take()
        } else {
            None
        }
    }

    pub fn set_unsent_packet(&self, message: PayloadWithChannel) {
        if let Ok(mut unsent_lock) = self.unsent_message.write() {
            *unsent_lock = Some(message);
        }
    }

    // Note: Even cloned, ther will be only one server side per session, so this is all fine.
    pub fn get_server_channels(&self) -> (PayloadWithChannelSender, PayloadWithChannelReceiver) {
        (self.tx_server.clone(), self.rx_server.clone())
    }

    pub fn get_proxy_channels(&self) -> (PayloadWithChannelSender, PayloadWithChannelReceiver) {
        (self.tx.clone(), self.rx.clone())
    }

    pub fn set_ip(&self, ip: SocketAddr) {
        if let Ok(mut ip_lock) = self.src_ip.write() {
            *ip_lock = ip;
        }
    }

    pub async fn start_server(&self) -> Result<ServerEndpoints> {
        self.server_running
            .store(true, std::sync::atomic::Ordering::Relaxed);

        self.session_proxy.start_server().await
    }

    pub(super) async fn stop_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.stop_server().await;
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
        self.server_running
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn server_tunnel_crypts(&self) -> Result<(crypt::Crypt, crypt::Crypt)> {
        crypt::tunnel::get_tunnel_crypts(&self.shared_secret, self.ticket(), self.seqs())
    }

    pub(super) async fn fail_server(&self) {
        self.server_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.session_proxy.fail_server().await;
    }

    pub(super) async fn stop_client(&self, stream_channel_id: u16) {
        self.session_proxy.stop_client(stream_channel_id).await;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        log::info!("Session dropped, stopping streams");
        self.stop.trigger();
    }
}
