use std::sync::OnceLock;

#[cfg(not(any(test, feature = "test-utils")))]
use anyhow::Result;
use zeroize::{Zeroize, ZeroizeOnDrop};

// reexport Ciphertext, PrivateKey, decapsulate, generate_keypair
pub use libcrux_ml_kem::{
    KEY_GENERATION_SEED_SIZE,
    mlkem768::{
        MlKem768Ciphertext as CipherText, MlKem768PrivateKey as PrivateKey, decapsulate,
        generate_key_pair,
    },
};

#[cfg(not(any(test, feature = "test-utils")))]
use rand::Rng;

// Note, changes to kem size (1024, 768 or 512) will need to update also PRIVATE_KEY_SIZE and CIPHERTEXT_SIZE
pub const PRIVATE_KEY_SIZE: usize = 2400;
pub const PUBLIC_KEY_SIZE: usize = 1184;
pub const CIPHERTEXT_SIZE: usize = 1088;

// `Copy` and `Clone` are intentionally NOT derived: the 2400-byte
// ML-KEM-768 private key must not be implicitly duplicated on every
// assignment. Callers that need the bytes take ownership of the
// `KeyPair` returned by `comms_keypair()` and move the arrays out
// field-by-field.
//
// `ZeroizeOnDrop` ensures both arrays are wiped when the keypair is
// dropped, so callers that fall out of scope with leftovers do not
// leave the secret sitting in freed memory.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct KeyPair {
    #[zeroize]
    pub private_key: [u8; PRIVATE_KEY_SIZE],
    #[zeroize]
    pub public_key: [u8; PUBLIC_KEY_SIZE],
}

// Process-wide KEM keypair used to decrypt the broker's ticket
// response. `OnceLock` lets us hand out a `&'static KeyPair` to
// production callers without `unsafe`, and the closure inside
// `get_or_init` decides (per build) whether to generate a fresh
// keypair or use the deterministic test one.
static KEM_KEYPAIR: OnceLock<KeyPair> = OnceLock::new();

/// Returns a borrow to the process-wide KEM keypair, initialised on
/// the first call. The slot is populated exactly once for the life of
/// the process: in production a random ML-KEM-768 keypair is generated
/// the first time we talk to the broker; under `cfg(test)` / the
/// `test-utils` feature we seed the slot with the deterministic
/// `kem::debug` keypair so integration tests have stable ciphertext to
/// assert against.
pub fn comms_keypair() -> &'static KeyPair {
    KEM_KEYPAIR.get_or_init(|| {
        #[cfg(any(test, feature = "test-utils"))]
        let keypair = {
            let (private_key, public_key) = crate::crypt::kem::debug::get_debug_kem_keypair_768();
            KeyPair {
                private_key,
                public_key,
            }
        };
        #[cfg(not(any(test, feature = "test-utils")))]
        let keypair = {
            let (private_key_vec, public_key_vec) =
                gen_keypair().expect("Failed to generate KEM keypair");
            KeyPair {
                private_key: private_key_vec
                    .try_into()
                    .expect("Invalid KEM private key size"),
                public_key: public_key_vec
                    .try_into()
                    .expect("Invalid KEM public key size"),
            }
        };
        keypair
    })
}

/// Generate a new KEM keypair (private key and public key)
#[cfg(not(any(test, feature = "test-utils")))]
fn gen_keypair() -> Result<(Vec<u8>, Vec<u8>)> {
    use rand::rngs::StdRng;

    let mut rng: StdRng = rand::make_rng();

    let mut randomness = [0u8; KEY_GENERATION_SEED_SIZE];
    rng.fill_bytes(&mut randomness);
    let keypair = generate_key_pair(randomness);
    Ok((
        keypair.private_key().as_slice().to_vec(),
        keypair.public_key().as_slice().to_vec(),
    ))
}

// Test-only module: contains a hardcoded ML-KEM-768 keypair used by the
// integration tests. Gated behind the `test-utils` feature (plus `cfg(test)`
// for in-crate tests) so the private key never ends up in the release
// binary's `.rodata`. Downstream crates enable the feature from their
// `[dev-dependencies]`.
#[cfg(any(test, feature = "test-utils"))]
pub mod debug;

#[cfg(test)]
mod tests {
    use super::*;

    /// The deterministic test keypair must agree with the constants
    /// `kem::debug` ships. This catches drift between the constants
    /// used at compile time and the `get_debug_kem_keypair_768`
    /// function that the `cfg(test)` branch of `comms_keypair` calls.
    #[test]
    fn test_debug_keypair_matches_constants() {
        let (private_key, public_key) = crate::crypt::kem::debug::get_debug_kem_keypair_768();
        assert_eq!(private_key.len(), PRIVATE_KEY_SIZE);
        assert_eq!(public_key.len(), PUBLIC_KEY_SIZE);
    }
}
