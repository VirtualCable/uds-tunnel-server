use anyhow::Result;

use aes_gcm::{AeadInPlace, Aes256Gcm, Nonce, aead::KeyInit};

pub mod consts;
pub mod types;

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
    /// The encrpyption is done inplace to avoid extra allocations.
    pub fn encrypt<'a>(
        &mut self,
        len: usize,
        buffer: &'a mut types::PacketBuffer,
    ) -> Result<&'a [u8]> {
        buffer.ensure_capacity(len + 16)?;

        let buffer = buffer.as_mut_slice();

        let counter = self.next_counter();
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        let aad = &counter.to_le_bytes();

        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), aad, &mut buffer[..len])
            .map_err(|e| anyhow::anyhow!("encryption failure: {:?}", e))?;
        buffer[len..len + 16].copy_from_slice(&tag);
        Ok(&buffer[..len + 16])
    }

    /// Decrypts the given ciphertext using AES-GCM with a nonce derived from the provided counter.
    /// The nonce is constructed by taking the counter value and padding it to 12 bytes with
    /// zeros. The counter value is also used as associated data (AAD) to ensure integrity.
    /// Returns the decrypted plaintext on success.
    pub fn decrypt<'a>(
        &mut self,
        counter: u64,
        length: u16,
        buffer: &'a mut types::PacketBuffer,
    ) -> Result<&'a [u8]> {
        let len = length as usize;
        let buffer = buffer.as_mut_slice();

        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        let aad = &counter.to_le_bytes();

        // Dividir el buffer en dos partes no solapadas
        let (ciphertext, rest) = buffer.split_at_mut(len);
        let tag = &rest[..16];

        self.cipher
            .decrypt_in_place_detached(Nonce::from_slice(&nonce), aad, ciphertext, tag.into())
            .map_err(|e| anyhow::anyhow!("decryption failure: {:?}", e))?;
        Ok(&buffer[..length as usize])
    }
}

pub fn parse_header(buffer: &[u8]) -> Result<(u64, u16)> {
    if buffer.len() < 10 {
        return Err(anyhow::anyhow!("buffer too small for header"));
    }
    let counter = u64::from_le_bytes(buffer[0..8].try_into().unwrap());
    let length = u16::from_le_bytes(buffer[8..10].try_into().unwrap());
    if length as usize > consts::MAX_PACKET_SIZE as usize {
        return Err(anyhow::anyhow!("invalid packet length: {}", length));
    }
    Ok((counter, length))
}

pub fn build_header(counter: u64, length: u16, buffer: &mut [u8]) -> Result<()> {
    if buffer.len() < 10 {
        return Err(anyhow::anyhow!("buffer too small for header"));
    }
    buffer[0..8].copy_from_slice(&counter.to_le_bytes());
    buffer[8..10].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    #[test]
    fn test_send_sync() {
        assert_send::<Crypt>();
        assert_sync::<Crypt>();
    }
}
