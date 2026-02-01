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

use shared::crypt::types::SharedSecret;

#[derive(serde::Deserialize, Debug)]
pub struct TicketRemote {
    pub host: String,
    pub port: u16,
}

#[derive(serde::Deserialize, Debug)]
pub struct TicketResponse {
    pub remotes: Vec<TicketRemote>,
    pub notify: String, // Stop notification ticket
    pub shared_secret: Option<String>,
}

impl TicketResponse {
    pub fn get_shared_secret(&self) -> Result<SharedSecret> {
        if let Some(ref secret_str) = self.shared_secret {
            SharedSecret::from_hex(secret_str)
        } else {
            Err(anyhow::anyhow!("Missing or invalid shared secret"))
        }
    }

    pub fn get_remotes_count(&self) -> usize {
        self.remotes.len()
    }

    pub async fn target_addr(&self, remote_id: u16) -> Result<String> {
        // Stream channel id is the index+1 of the remotes array
        if remote_id as usize >= self.remotes.len() {
            return Err(anyhow::anyhow!("Invalid stream_channel_id: {}", remote_id));
        }
        Ok(format!(
            "{}:{}",
            self.remotes[remote_id as usize].host, self.remotes[remote_id as usize].port
        ))
    }

    pub fn validate(&self) -> Result<()> {
        if self.remotes.is_empty() {
            return Err(anyhow::anyhow!("No remotes in ticket response"));
        }
        for remote in &self.remotes {
            if remote.host.is_empty() || remote.port == 0 {
                return Err(anyhow::anyhow!(
                    "Invalid remote in ticket response: {:?}",
                    remote
                ));
            }
        }
        Ok(())
    }
}
