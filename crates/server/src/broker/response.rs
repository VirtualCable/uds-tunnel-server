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
