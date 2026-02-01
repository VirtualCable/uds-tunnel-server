use std::sync::OnceLock;

use anyhow::Result;

use rand::{TryRngCore, rngs::OsRng};

// reexport Ciphertext, PrivateKey, decapsulate, generate_keypair
pub use libcrux_ml_kem::mlkem768::{
    MlKem768Ciphertext as CipherText, MlKem768PrivateKey as PrivateKey, decapsulate,
};

use libcrux_ml_kem::mlkem768::generate_key_pair as ml_kem_generate_key_pair;

// Note, changes to kem size (1024, 768 or 512) will need to update also PRIVATE_KEY_SIZE and CIPHERTEXT_SIZE
pub const PRIVATE_KEY_SIZE: usize = 2400;
pub const PUBLIC_KEY_SIZE: usize = 1184;
pub const CIPHERTEXT_SIZE: usize = 1088;

pub struct KeyPair {
    pub private_key: [u8; PRIVATE_KEY_SIZE],
    pub public_key: [u8; PUBLIC_KEY_SIZE],
}

// Static public/private keys for ticket decryption
static KEM_KEYPAIR: OnceLock<KeyPair> = OnceLock::new();

pub fn comms_keypair() -> &'static KeyPair {
    // Without keys, we cannot proceed
    KEM_KEYPAIR.get_or_init(|| {
        let (private_key, public_key) =
            generate_key_pair().expect("Failed to generate KEM keypair");
        KeyPair {
            private_key: private_key.try_into().expect("Invalid private key length"),
            public_key: public_key.try_into().expect("Invalid public key length"),
        }
    })
}

/// Generate a new KEM keypair (private key and public key)
fn generate_key_pair() -> Result<(Vec<u8>, Vec<u8>)> {
    let mut rng = OsRng;
    let mut randomness = [0u8; 64];
    rng.try_fill_bytes(&mut randomness)
        .map_err(|e| anyhow::format_err!("Failed to generate randomness: {}", e))?;
    let keypair = ml_kem_generate_key_pair(randomness);
    Ok((
        keypair.private_key().as_slice().to_vec(),
        keypair.public_key().as_slice().to_vec(),
    ))
}
