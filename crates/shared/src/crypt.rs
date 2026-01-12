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

    pub fn next_counter(&mut self) -> u64 {
        let counter = self.counter;
        self.counter += 1;
        counter
    }

    pub fn get_counter(&self) -> u64 {
        self.counter
    }

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
