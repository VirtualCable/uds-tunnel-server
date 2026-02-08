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
use tokio::net::TcpStream;

use super::types;

use crate::{session::Session, stream::client::TunnelClientStream};
use shared::{log, protocol, system::trigger::Trigger};

#[derive(Debug, Clone)]
struct ClientInfo {
    sender: protocol::PayloadSender,
    stop: Trigger,
}

pub(super) struct ClientFanIn {
    clients_senders: Vec<Option<ClientInfo>>,
    sender: protocol::PayloadWithChannelSender,
    receiver: protocol::PayloadWithChannelReceiver,
}

impl ClientFanIn {
    pub fn new() -> Self {
        let (sender, receiver) = protocol::payload_with_channel_pair();
        Self {
            clients_senders: Vec::new(),
            sender,
            receiver,
        }
    }

    pub async fn create_client(
        &mut self,
        stream_channel_id: u16,
        session: Arc<Session>,
    ) -> Result<()> {
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

        let (sender, receiver) = protocol::payload_pair();
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
            types::ClientEndpoints {
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

    pub async fn send_to_channel(&self, msg: protocol::PayloadWithChannel) -> Result<()> {
        if msg.channel_id == 0 || msg.channel_id as usize > self.clients_senders.len() {
            return Err(anyhow::anyhow!(
                "Invalid stream_channel_id: {}",
                msg.channel_id
            ));
        }
        if let Some(client) = &self.clients_senders[(msg.channel_id - 1) as usize] {
            client.sender.send_async(msg.payload).await?;
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

    pub async fn recv(&self) -> Result<protocol::PayloadWithChannel> {
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
