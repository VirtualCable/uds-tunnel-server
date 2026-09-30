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
struct ClientChannel {
    sender: protocol::PayloadSender,
    stop: Trigger,
    /// Identity of the generation that currently owns this slot. A client
    /// stream is spawned with its generation and reports it back when it
    /// dies; the proxy only tears the slot down when the reported
    /// generation still matches the slot. Without it, reopening a channel
    /// (`create_client` replacing a live slot) let the *previous*
    /// generation's death notice stop and clear the *new* occupant — the
    /// reopened channel was killed and its frames silently dropped.
    generation: u64,
}

pub(super) struct ClientChannels {
    clients_senders: Vec<Option<ClientChannel>>,
    sender: protocol::PayloadWithChannelSender,
    receiver: protocol::PayloadWithChannelReceiver,
    /// Monotonic generation counter: each `create_client` mints a fresh id
    /// that tags the slot and the stream it spawns, so a stale stream's
    /// teardown can be matched against the slot it used to own.
    next_generation: u64,
}

impl ClientChannels {
    pub fn new() -> Self {
        let (sender, receiver) = protocol::payload_with_channel_pair();
        Self {
            clients_senders: Vec::new(),
            sender,
            receiver,
            next_generation: 0,
        }
    }

    /// Number of slots currently allocated in `clients_senders`. Test-only.
    #[cfg(test)]
    pub(super) fn slots_len(&self) -> usize {
        self.clients_senders.len()
    }

    /// True when the slot for `stream_channel_id` is currently owned by the
    /// generation `generation`. A death report from any other generation
    /// (or for an empty slot) is stale: the channel was already closed or
    /// reopened by a newer stream.
    pub(super) fn generation_matches(&self, stream_channel_id: u16, generation: u64) -> bool {
        if stream_channel_id == 0 || stream_channel_id as usize > self.clients_senders.len() {
            return false;
        }
        self.clients_senders[(stream_channel_id - 1) as usize]
            .as_ref()
            .is_some_and(|client| client.generation == generation)
    }

    pub async fn create_client(
        &mut self,
        stream_channel_id: u16,
        session: Arc<Session>,
    ) -> Result<()> {
        // Reject channel id 0 (reserved for the control channel) and any id
        // outside the legitimate remote count returned by the broker. Without
        // these checks a malicious or buggy client could grow
        // `clients_senders` up to `u16::MAX` slots (memory DoS) or index
        // `session.remotes` out of bounds (panic).
        if stream_channel_id == 0 {
            anyhow::bail!("Invalid stream_channel_id: 0 is reserved for the control channel");
        }
        if stream_channel_id > shared::protocol::consts::MAX_CHANNEL_ID {
            anyhow::bail!(
                "Invalid stream_channel_id: {} exceeds hard cap of {}",
                stream_channel_id,
                shared::protocol::consts::MAX_CHANNEL_ID
            );
        }
        let idx = (stream_channel_id - 1) as usize;
        if idx >= session.remotes.len() {
            anyhow::bail!(
                "Invalid stream_channel_id: {} (only {} remotes available for this session)",
                stream_channel_id,
                session.remotes.len()
            );
        }

        // Ensure vector is large enough, but cap the growth at the
        // legitimate remote count for this session.
        if self.clients_senders.len() <= idx {
            self.clients_senders.resize(idx + 1, None);
        }

        // If current client is Some, we are replacing it, so ensure old one receives the stop signal
        if let Some(old_client) = &self.clients_senders[idx] {
            // Ensure notify old client to stop before replacing
            old_client.stop.trigger();
        }

        let (sender, receiver) = protocol::payload_pair();
        // (self.sender.clone(), receiver)

        // `idx` is guaranteed in range by the checks above.
        let target_stream = TcpStream::connect(&session.remotes[idx]).await?;

        // Split the target stream into reader and writer
        let (target_reader, target_writer) = target_stream.into_split();

        let stop = Trigger::new();
        let generation = self.next_generation;
        self.next_generation += 1;

        // Note: The TunnelClientStream will not receive the global stop, but its own stop trigger
        // managed by the ClientFanIn
        let client_stream = TunnelClientStream::new(
            *session.id(),
            stop.clone(),
            stream_channel_id,
            generation,
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

        self.clients_senders[idx] = Some(ClientChannel {
            sender,
            stop,
            generation,
        });
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

    pub async fn recv(&mut self) -> Result<protocol::PayloadWithChannel> {
        self.receiver
            .recv_async()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to receive from client channels: {}", e))
    }

    /// Closes the client for the given stream_channel_id
    pub fn close_client(&mut self, stream_channel_id: u16) {
        // Channel 0 is the control channel and there is no client slot for
        // it; without this guard `(stream_channel_id - 1)` underflows
        // (panic in debug, index 65535 out of bounds in release) and the
        // resulting panic aborts the spawned proxy task before its
        // `stop.trigger()` + `remove_session` cleanup, leaking the session.
        if stream_channel_id == 0 || stream_channel_id as usize > self.clients_senders.len() {
            return;
        }
        self.clients_senders[(stream_channel_id - 1) as usize] = None;
    }
}
