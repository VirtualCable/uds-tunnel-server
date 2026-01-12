use anyhow::Result;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit, Payload},
};
pub struct Crypt {
    cipher: Aes256Gcm,
    counter: u64,
}

impl Crypt {
    pub fn new(key: &[u8; 32]) -> Self {
        let cipher = Aes256Gcm::new(key.into());
        Crypt { cipher, counter: 0 }
    }

    /// Increments and returns the current counter value.
    /// Note: the encrypt method automatically calls this method to get a unique counter for each encryption.
    /// Returns the incremented counter value.
    pub fn next_counter(&mut self) -> u64 {
        let counter = self.counter;
        self.counter += 1;
        counter
    }

    /// Returns the current counter value without incrementing it.
    pub fn get_counter(&self) -> u64 {
        self.counter
    }

    /// Encrypts the given plaintext using AES-GCM with a unique nonce derived from an internal counter.
    /// The nonce is constructed by taking the current counter value and padding it to 12 bytes
    /// with zeros. The counter value is also used as associated data (AAD) to ensure integrity.
    /// Returns the ciphertext on success.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let counter = self.next_counter();
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        let aad = &counter.to_le_bytes();

        self.cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|e| anyhow::anyhow!("encryption failure: {:?}", e))
    }

    /// Decrypts the given ciphertext using AES-GCM with a nonce derived from the provided counter.
    /// The nonce is constructed by taking the counter value and padding it to 12 bytes with
    /// zeros. The counter value is also used as associated data (AAD) to ensure integrity.
    /// Returns the decrypted plaintext on success.
    pub fn decrypt(&mut self, ciphertext: &[u8], counter: u64) -> Result<Vec<u8>> {
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        let aad = &counter.to_le_bytes();

        self.cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .map_err(|e| anyhow::anyhow!("decryption failure: {:?}", e))
    }
}
