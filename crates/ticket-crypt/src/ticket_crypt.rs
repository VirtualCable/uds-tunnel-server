use anyhow::Result;

use hkdf::Hkdf;
use sha2::{Sha256, digest::typenum};

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine as _, engine::general_purpose};

use libcrux_ml_kem::mlkem768::{
    MlKem768Ciphertext as CipherText, MlKem768PrivateKey as PrivateKey, decapsulate,
};

// Note, changes to kem size (1024, 768 or 512) will need to update also SECRET_KEY_SIZE and CIPHERTEXT_SIZE
pub const SECRET_KEY_SIZE: usize = 2400;
pub const CIPHERTEXT_SIZE: usize = 1088;

struct TunnelMaterial {
    pub key_payload: [u8; 32],
    pub key_send: [u8; 32],
    pub key_receive: [u8; 32],
    pub nonce_send: [u8; 12],
    pub nonce_receive: [u8; 12],
}

fn derive_tunnel_material(shared_secret: &[u8], ticket_id: &[u8]) -> Result<TunnelMaterial> {
    if ticket_id.len() < 48 {
        anyhow::bail!("ticket_id must be at least 48 bytes");
    }

    // HKDF-Extract + Expand with SHA-256
    let hk = Hkdf::<Sha256>::new(Some(ticket_id), shared_secret);

    let mut okm = [0u8; 120];
    hk.expand(b"openuds-ticket-crypt", &mut okm)
        .map_err(|_| anyhow::format_err!("HKDF expand failed"))?;

    let mut key_payload = [0u8; 32];
    let mut key_send = [0u8; 32];
    let mut key_receive = [0u8; 32];
    let mut nonce_send = [0u8; 12];
    let mut nonce_receive = [0u8; 12];

    key_payload.copy_from_slice(&okm[0..32]);
    key_send.copy_from_slice(&okm[32..64]);
    key_receive.copy_from_slice(&okm[64..96]);
    nonce_send.copy_from_slice(&okm[96..108]);
    nonce_receive.copy_from_slice(&okm[108..120]);

    Ok(TunnelMaterial {
        key_payload,
        key_send,
        key_receive,
        nonce_send,
        nonce_receive,
    })
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct Ticket {
    pub algorithm: String,
    pub ciphertext: String,
    pub data: String,
}

impl Ticket {
    pub fn new(algorithm: &str, ciphertext: &str, data: &str) -> Self {
        Ticket {
            algorithm: algorithm.to_string(),
            ciphertext: ciphertext.to_string(),
            data: data.to_string(),
        }
    }

    pub fn recover_data_from_json(
        &self,
        ticket_id: &[u8],
        kem_private_key: &[u8; SECRET_KEY_SIZE],
    ) -> Result<serde_json::Value> {
        let kem_private_key = PrivateKey::from(kem_private_key);

        // Extract shared_secret from KEM ciphertext
        let kem_ciphertext_bytes: [u8; CIPHERTEXT_SIZE] = general_purpose::STANDARD
            .decode(&self.ciphertext)
            .map_err(|e| anyhow::format_err!("Failed to decode base64 ciphertext: {}", e))?
            .try_into()
            .map_err(|_| anyhow::format_err!("Invalid ciphertext size"))?;

        let kem_ciphertext = CipherText::from(&kem_ciphertext_bytes);
        // Note, the opoeration will always succeed, even for invalid ciphertexts
        // As long as the sizes are correct (that will bee for sure)
        let shared_secret = decapsulate(&kem_private_key, &kem_ciphertext);

        let data = general_purpose::STANDARD
            .decode(&self.data)
            .map_err(|e| anyhow::format_err!("Failed to decode base64 data: {}", e))?;

        // Derive tunnel material
        let material = derive_tunnel_material(&shared_secret, ticket_id)?;

        let cipher = Aes256Gcm::new(material.key_payload.as_ref().into());
        let nonce: &Nonce<typenum::U12> = Nonce::from_slice(material.nonce_send.as_ref());
        let plaintext = cipher
            .decrypt(nonce, data.as_ref())
            .map_err(|_| anyhow::format_err!("AES-256-GCM decryption failed"))?;
        let mut json_value: serde_json::Value = serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::format_err!("Failed to parse JSON from decrypted data"))?;

        // Add key_send, key_receive, nonce_send, nonce_receive to the json_value
        if let serde_json::Value::Object(ref mut map) = json_value {
            map.insert(
                "key_send".to_string(),
                serde_json::to_value(material.key_send)?,
            );
            map.insert(
                "key_receive".to_string(),
                serde_json::to_value(material.key_receive)?,
            );
            map.insert(
                "nonce_send".to_string(),
                serde_json::to_value(material.nonce_send)?,
            );
            map.insert(
                "nonce_receive".to_string(),
                serde_json::to_value(material.nonce_receive)?,
            );
        } else {
            anyhow::bail!("JSON value is not an object");
        }

        Ok(json_value)
    }
}
    