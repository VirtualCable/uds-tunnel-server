use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use sha2::digest::typenum;

use crate::{
    crypt::{
        kem::{CIPHERTEXT_SIZE, CipherText, PRIVATE_KEY_SIZE, PrivateKey, decapsulate},
        tunnel::derive_tunnel_material,
    },
    ticket::Ticket,
};

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct EncryptedTicketResponse {
    pub algorithm: String,
    pub ciphertext: String,
    pub data: String,
}

impl EncryptedTicketResponse {
    pub fn new(algorithm: &str, ciphertext: &str, data: &str) -> Self {
        EncryptedTicketResponse {
            algorithm: algorithm.to_string(),
            ciphertext: ciphertext.to_string(),
            data: data.to_string(),
        }
    }

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

        let cipher = Aes256Gcm::new(material.key_payload.as_ref().into());
        let nonce: &Nonce<typenum::U12> = Nonce::from_slice(material.nonce_payload.as_ref());
        let plaintext = cipher
            .decrypt(nonce, data.as_ref())
            .map_err(|_| anyhow::format_err!("AES-256-GCM decryption failed"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::format_err!("Failed to parse JSON from decrypted data"))
    }
}
