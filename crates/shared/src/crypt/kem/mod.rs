use std::sync::RwLock;

use anyhow::Result;

// reexport Ciphertext, PrivateKey, decapsulate, generate_keypair
pub use libcrux_ml_kem::mlkem768::{
    MlKem768Ciphertext as CipherText, MlKem768PrivateKey as PrivateKey, decapsulate,
    generate_key_pair,
};

// Note, changes to kem size (1024, 768 or 512) will need to update also PRIVATE_KEY_SIZE and CIPHERTEXT_SIZE
pub const PRIVATE_KEY_SIZE: usize = 2400;
pub const PUBLIC_KEY_SIZE: usize = 1184;
pub const CIPHERTEXT_SIZE: usize = 1088;

#[derive(Clone, Copy)]
pub struct KeyPair {
    pub private_key: [u8; PRIVATE_KEY_SIZE],
    pub public_key: [u8; PUBLIC_KEY_SIZE],
}

// Static public/private keys for ticket decryption
static KEM_KEYPAIR: RwLock<Option<KeyPair>> = RwLock::new(None);

pub fn comms_keypair() -> KeyPair {
    // Without keys, we cannot proceed, so any failure here is fatal
    {
        let kem_keypair = KEM_KEYPAIR.read().unwrap();
        if let Some(ref keys) = *kem_keypair {
            return *keys;
        }
    }

    let (private_key_vec, public_key_vec) = gen_keypair().expect("Failed to generate KEM keypair");
    let mut kem_keypair = KEM_KEYPAIR.write().unwrap();
    let keys = KeyPair {
        private_key: private_key_vec
            .try_into()
            .expect("Invalid KEM private key size"),
        public_key: public_key_vec
            .try_into()
            .expect("Invalid KEM public key size"),
    };
    *kem_keypair = Some(keys);
    keys
}

pub fn set_comms_keypair(private_key: [u8; PRIVATE_KEY_SIZE], public_key: [u8; PUBLIC_KEY_SIZE]) {
    let mut kem_keypair = KEM_KEYPAIR.write().unwrap();
    let keys = KeyPair {
        private_key,
        public_key,
    };
    *kem_keypair = Some(keys);
}

/// Generate a new KEM keypair (private key and public key)
fn gen_keypair() -> Result<(Vec<u8>, Vec<u8>)> {
    use rand::{TryRngCore, rngs::OsRng};

    let mut rng = OsRng;
    let mut randomness = [0u8; 64];
    rng.try_fill_bytes(&mut randomness)
        .map_err(|e| anyhow::format_err!("Failed to generate randomness: {}", e))?;
    let keypair = generate_key_pair(randomness);
    Ok((
        keypair.private_key().as_slice().to_vec(),
        keypair.public_key().as_slice().to_vec(),
    ))
}

#[cfg(debug_assertions)]
pub mod debug;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gen_keypair() {
        let (private_key, public_key) = gen_keypair().expect("Failed to generate KEM keypair");
        assert_eq!(private_key.len(), PRIVATE_KEY_SIZE);
        assert_eq!(public_key.len(), PUBLIC_KEY_SIZE);
    }
}
