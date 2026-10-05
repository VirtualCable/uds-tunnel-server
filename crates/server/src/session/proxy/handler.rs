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
use flume::Sender;

use shared::log;

use super::types;

pub(super) enum Command {
    AttachServer {
        reply: Sender<types::ServerEndpoints>,
    },
    ServerFailed,  // Will not close the proxy, to allow recovery
    ServerStopped, // Will close the proxy, as the server is done
    // Client is attached by us, so no need for an attach command
    // stream_channel_id + the generation that owns it. The proxy only stops
    // and closes the slot when the generation still matches, so a stale
    // stream (its channel already replaced by a newer generation) cannot
    // tear down the new occupant. No need to know if it failed or stopped
    // normally.
    ClientStopped(u16, u64),
}

#[derive(Debug)]
pub struct Handler {
    ctrl_tx: Sender<Command>,
}

impl Handler {
    pub(super) fn new(ctrl_tx: Sender<Command>) -> Self {
        Self { ctrl_tx }
    }

    /// Upper bound for the proxy to answer an attach request. The proxy
    /// drains queued replies on exit (so `reply_rx` normally disconnects
    /// immediately), making this a purely defensive guard against a wedged
    /// proxy that would otherwise strand the caller forever.
    const ATTACH_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    pub async fn start_server(&self) -> Result<types::ServerEndpoints> {
        log::debug!("Starting server in session proxy");
        let (reply_tx, reply_rx) = flume::bounded(1);
        let cmd = Command::AttachServer { reply: reply_tx };
        self.ctrl_tx.send_async(cmd).await?;
        let endpoints = tokio::time::timeout(Self::ATTACH_REPLY_TIMEOUT, reply_rx.recv_async())
            .await
            .map_err(|_| {
                anyhow::anyhow!("Timed out waiting for session proxy to attach the server")
            })?
            .map_err(|_| anyhow::anyhow!("Session proxy is not accepting attach requests"))?;
        Ok(endpoints)
    }

    pub async fn stop_server(&self) {
        if let Err(e) = self.ctrl_tx.send_async(Command::ServerStopped).await {
            log::error!(
                "Failed to send stop server command to session proxy: {:?}",
                e
            );
        }
    }

    pub async fn fail_server(&self) {
        if let Err(e) = self.ctrl_tx.send_async(Command::ServerFailed).await {
            log::error!(
                "Failed to send fail server command to session proxy: {:?}",
                e
            );
        }
    }

    pub async fn stop_client(&self, stream_channel_id: u16, generation: u64) {
        if let Err(e) = self
            .ctrl_tx
            .send_async(Command::ClientStopped(stream_channel_id, generation))
            .await
        {
            log::error!(
                "Failed to send stop client command to session proxy: {:?}",
                e
            );
        }
    }
}
