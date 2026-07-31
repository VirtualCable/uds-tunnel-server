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
use base64::{Engine as _, engine::general_purpose};

use shared::{
    crypt::{
        Crypt,
        kem::{CIPHERTEXT_SIZE, CipherText, PRIVATE_KEY_SIZE, PrivateKey, decapsulate},
        tunnel::derive_tunnel_material,
        types::SharedSecret,
    },
    protocol::ticket::Ticket,
};

#[derive(serde::Deserialize, Debug)]
pub struct TicketRemote {
    pub host: String,
    pub port: u16,
    // Also has an optional "extra" field that can contain any additional information as a JSON object
    // Currently, we ignore it buy this tunnel
    // pub extra: Option<serde_json::Value>,
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

    pub fn channels_remotes(&self) -> Vec<String> {
        self.remotes
            .iter()
            .map(|r| format!("{}:{}", r.host, r.port))
            .collect()
    }

    pub fn remotes_count(&self) -> usize {
        self.remotes.len()
    }

    pub fn validate(&self) -> Result<()> {
        if self.remotes.is_empty() {
            return Err(anyhow::anyhow!("No remotes in ticket response"));
        }
        // Defense in depth: the server enforces a hard cap on the number of
        // channels per session (see shared::protocol::consts::MAX_CHANNEL_ID).
        // A broker that returns more remotes than the cap would have us open
        // a session we cannot honour, so reject it up front and close the
        // connection rather than silently truncate.
        if self.remotes.len() > shared::protocol::consts::MAX_CHANNEL_ID as usize {
            return Err(anyhow::anyhow!(
                "Too many remotes in ticket response: {} (max {})",
                self.remotes.len(),
                shared::protocol::consts::MAX_CHANNEL_ID
            ));
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

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub(super) struct EncryptedTicketResponse {
    pub algorithm: String,
    pub ciphertext: String,
    pub data: String,
}

impl EncryptedTicketResponse {
    pub fn recover_data_from_json(
        &self,
        ticket_id: &Ticket,
        private_key: &[u8; PRIVATE_KEY_SIZE],
    ) -> Result<serde_json::Value> {
        let kem_private_key = PrivateKey::from(private_key);

        // Extract shared_secret from KEM ciphertext
        let kem_ciphertext_bytes: [u8; CIPHERTEXT_SIZE] = general_purpose::STANDARD
            .decode(&self.ciphertext)
            .map_err(|e| anyhow::format_err!("Failed to decode base64 ciphertext: {}", e))?
            .try_into()
            .map_err(|_| anyhow::format_err!("Invalid ciphertext size"))?;

        let kem_ciphertext = CipherText::from(&kem_ciphertext_bytes);
        // Note, the opoeration will always succeed, even for invalid ciphertexts
        // As long as the sizes are correct (that will bee for sure)
        let shared_secret = decapsulate(&kem_private_key, &kem_ciphertext).into();

        let data = general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|e| anyhow::format_err!("Failed to decode base64 data: {}", e))?;

        // Derive tunnel material
        let material = derive_tunnel_material(&shared_secret, ticket_id)?;
        let plaintext =
            Crypt::simple_decrypt(&material.key_payload, &material.nonce_payload, &data)?;

        serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::format_err!("Failed to parse JSON from decrypted data"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response_with_n_remotes(n: usize) -> TicketResponse {
        let remotes = (0..n)
            .map(|i| TicketRemote {
                host: format!("host{i}.example.com"),
                port: 10000 + i as u16,
            })
            .collect();
        TicketResponse {
            remotes,
            notify: String::new(),
            shared_secret: None,
        }
    }

    #[test]
    fn validate_rejects_empty_remotes() {
        let resp = make_response_with_n_remotes(0);
        assert!(resp.validate().is_err());
    }

    #[test]
    fn validate_accepts_remotes_up_to_max_channel_id() {
        use shared::protocol::consts::MAX_CHANNEL_ID;
        let resp = make_response_with_n_remotes(MAX_CHANNEL_ID as usize);
        assert!(
            resp.validate().is_ok(),
            "should accept exactly MAX_CHANNEL_ID"
        );
    }

    #[test]
    fn validate_rejects_more_remotes_than_max_channel_id() {
        use shared::protocol::consts::MAX_CHANNEL_ID;
        let resp = make_response_with_n_remotes(MAX_CHANNEL_ID as usize + 1);
        let err = resp.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Too many remotes") && msg.contains("max"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn validate_rejects_empty_host_or_zero_port() {
        let mut resp = make_response_with_n_remotes(1);
        resp.remotes[0].host = String::new();
        assert!(resp.validate().is_err());

        let mut resp = make_response_with_n_remotes(1);
        resp.remotes[0].port = 0;
        assert!(resp.validate().is_err());
    }
}
