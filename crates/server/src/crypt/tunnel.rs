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

use anyhow::Result;

struct Material {
    pub _not_used: [u8; 32],
    pub key_receive: [u8; 32],
    pub key_send: [u8; 32],
}

use super::Crypt;
use hkdf::Hkdf;
use sha2::Sha256;

fn derive_tunnel_material(shared_secret: &[u8], ticket_id: &[u8]) -> Result<Material> {
    if ticket_id.len() < 48 {
        anyhow::bail!("ticket_id must be at least 48 bytes");
    }

    // HKDF-Extract + Expand with SHA-256
    let hk = Hkdf::<Sha256>::new(Some(ticket_id), shared_secret);

    let mut okm = [0u8; 96];
    hk.expand(b"openuds-ticket-crypt", &mut okm)
        .map_err(|_| anyhow::format_err!("HKDF expand failed"))?;

    let mut _not_used = [0u8; 32];
    let mut key_send = [0u8; 32];
    let mut key_receive = [0u8; 32];

    _not_used.copy_from_slice(&okm[0..32]);
    key_send.copy_from_slice(&okm[32..64]);
    key_receive.copy_from_slice(&okm[64..96]);

    Ok(Material {
        _not_used,
        key_receive: key_send,
        key_send: key_receive,
    })
}

pub fn get_tunnel_crypts(shared_secret: &[u8], ticket_id: &[u8]) -> Result<(Crypt, Crypt)> {
    let material = derive_tunnel_material(shared_secret, ticket_id)?;

    let inbound = Crypt::new(&material.key_send);
    let outbound = Crypt::new(&material.key_receive);

    Ok((inbound, outbound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_tunnel_material() {
        let shared_secret = [1u8; 32];
        let ticket_id = [2u8; 48];

        let material = derive_tunnel_material(&shared_secret, &ticket_id).unwrap();

        // Verify derived keys, known values
        assert_eq!(
            material.key_receive,
            [
                165, 213, 31, 20, 62, 238, 14, 209, 50, 193, 226, 239, 216, 45, 76, 37, 101, 11,
                173, 113, 185, 254, 51, 7, 50, 39, 232, 253, 55, 12, 21, 156
            ]
        );
        assert_eq!(
            material.key_send,
            [
                30, 79, 83, 235, 53, 71, 186, 71, 34, 250, 3, 51, 222, 193, 90, 208, 48, 112, 207,
                208, 219, 166, 191, 4, 208, 106, 159, 121, 221, 115, 30, 174
            ]
        );
    }

    #[test]
    fn test_get_tunnel_crypts() {
        let shared_secret = [1u8; 32];
        let ticket_id = [2u8; 48];

        let (inbound, outbound) = get_tunnel_crypts(&shared_secret, &ticket_id).unwrap();

        assert_eq!(inbound.current_seq(), 0);
        assert_eq!(outbound.current_seq(), 0);
    }

    #[test]
    fn test_invalid_ticket_length() {
        let shared_secret = [1u8; 32];
        let ticket_id = [2u8; 16]; // Too short

        let result = get_tunnel_crypts(&shared_secret, &ticket_id);
        assert!(result.is_err());
    }
}
