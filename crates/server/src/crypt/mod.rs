use anyhow::Result;

use aes_gcm::{AeadInPlace, Aes256Gcm, Nonce, aead::KeyInit};

pub mod consts;
pub mod types;

pub struct Crypt {
    cipher: Aes256Gcm,
    seq: u64,
}

impl Crypt {
    pub fn new(key: &[u8; 32]) -> Self {
        let cipher = Aes256Gcm::new(key.into());
        Crypt { cipher, seq: 0 }
    }

    /// Increments and returns the internal seq.
    /// Note: the encrypt method automatically calls this method to get a unique seq for each encryption.
    /// Returns the incremented seq value.
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Returns the current seq value without incrementing it.
    pub fn current_seq(&self) -> u64 {
        self.seq
    }

    /// Encrypts the given plaintext using AES-GCM with a unique nonce derived from an internal seq.
    /// The nonce is constructed by taking the current seq value and padding it to 12 bytes
    /// with zeros. The seq value is also used as associated data (AAD) to ensure integrity.
    /// Returns the ciphertext on success.
    /// The encrpyption is done inplace to avoid extra allocations.
    pub fn encrypt<'a>(
        &mut self,
        len: usize,
        buffer: &'a mut types::PacketBuffer,
    ) -> Result<&'a [u8]> {
        buffer.ensure_capacity(len + 16)?;

        let buffer = buffer.as_mut_slice();

        let seq = self.next_seq();
        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&seq.to_le_bytes());
        let aad = &seq.to_le_bytes();

        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), aad, &mut buffer[..len])
            .map_err(|e| anyhow::anyhow!("encryption failure: {:?}", e))?;
        buffer[len..len + 16].copy_from_slice(&tag);
        Ok(&buffer[..len + 16])
    }

    /// Decrypts the given ciphertext using AES-GCM with a nonce derived from the provided seq.
    /// The nonce is constructed by taking the seq value and padding it to 12 bytes with
    /// zeros. The seq value is also used as associated data (AAD) to ensure integrity.
    /// Returns the decrypted plaintext on success.
    pub fn decrypt<'a>(
        &mut self,
        seq: u64,
        length: u16,
        buffer: &'a mut types::PacketBuffer,
    ) -> Result<&'a [u8]> {
        if seq < self.seq {
            return Err(anyhow::anyhow!(
                "replay attack detected: seq {} < current {}",
                seq,
                self.seq
            ));
        }
        self.seq = seq + 1; // Update to last used seq + 1, so no replays are possible

        let len = length as usize;
        let buffer = buffer.as_mut_slice();

        let mut nonce = [0; 12];
        nonce[..8].copy_from_slice(&seq.to_le_bytes());
        let aad = &seq.to_le_bytes();

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
    let seq = u64::from_le_bytes(buffer[0..8].try_into().unwrap());
    let length = u16::from_le_bytes(buffer[8..10].try_into().unwrap());
    if length as usize > consts::MAX_PACKET_SIZE as usize {
        return Err(anyhow::anyhow!("invalid packet length: {}", length));
    }
    Ok((seq, length))
}

pub fn build_header(seq: u64, length: u16, buffer: &mut [u8]) -> Result<()> {
    if buffer.len() < 10 {
        return Err(anyhow::anyhow!("buffer too small for header"));
    }
    buffer[0..8].copy_from_slice(&seq.to_le_bytes());
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

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = [7u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf = types::PacketBuffer::new();
        let plaintext = b"hello world!";
        buf.copy_from_slice(plaintext).unwrap();

        let ciphertext = crypt.encrypt(plaintext.len(), &mut buf).unwrap();

        // Now decrypt
        let seq = crypt.current_seq();
        let length = plaintext.len() as u16;

        let mut buf2 = types::PacketBuffer::from(ciphertext);
        let decrypted = crypt.decrypt(seq, length, &mut buf2).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_sequence_increments() {
        let key = [1u8; 32];
        let mut crypt = Crypt::new(&key);

        assert_eq!(crypt.current_seq(), 0);
        assert_eq!(crypt.next_seq(), 1);
        assert_eq!(crypt.next_seq(), 2);
        assert_eq!(crypt.current_seq(), 2);
    }

    #[test]
    fn test_replay_rejected() {
        let key = [2u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf = types::PacketBuffer::new();
        buf.copy_from_slice(b"abc").unwrap();

        let ciphertext = crypt.encrypt(3, &mut buf).unwrap();
        let seq = crypt.current_seq();

        let mut buf2 = types::PacketBuffer::from(ciphertext);

        // Primer decrypt OK
        crypt.decrypt(seq, 3, &mut buf2).unwrap();

        // Segundo decrypt con el mismo seq debe fallar
        let mut buf3 = types::PacketBuffer::from(ciphertext);
        let err = crypt.decrypt(seq, 3, &mut buf3).unwrap_err();

        assert!(err.to_string().contains("replay attack"));
    }

    #[test]
    fn test_decrypt_fails_on_bad_tag() {
        let key = [3u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf = types::PacketBuffer::new();
        buf.copy_from_slice(b"hola").unwrap();

        let ciphertext = crypt.encrypt(4, &mut buf).unwrap();
        let seq = crypt.current_seq();

        let mut corrupted = ciphertext.to_vec();
        corrupted[ciphertext.len() - 1] ^= 0xFF; // flip bit

        let mut buf2 = types::PacketBuffer::from(&corrupted[..]);
        let err = crypt.decrypt(seq, 4, &mut buf2).unwrap_err();

        assert!(err.to_string().contains("decryption failure"));
    }

    #[test]
    fn test_decrypt_fails_on_truncated_ciphertext() {
        let key = [4u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf = types::PacketBuffer::new();
        buf.copy_from_slice(b"hola").unwrap();

        let ciphertext = crypt.encrypt(4, &mut buf).unwrap();
        let seq = crypt.current_seq();

        let truncated = ciphertext[..ciphertext.len() - 5].to_vec();

        let mut buf2 = types::PacketBuffer::from(&truncated[..]);
        let err = crypt.decrypt(seq, 4, &mut buf2).unwrap_err();

        assert!(err.to_string().contains("decryption failure"));
    }

    #[test]
    fn test_parse_header() {
        let mut buf = [0u8; 10];
        build_header(123, 456, &mut buf).unwrap();

        let (seq, len) = parse_header(&buf).unwrap();
        assert_eq!(seq, 123);
        assert_eq!(len, 456);
    }

    #[test]
    fn test_parse_header_invalid_size() {
        let buf = [0u8; 5];
        assert!(parse_header(&buf).is_err());
    }

    #[test]
    fn test_build_header() {
        let mut buf = [0u8; 10];
        build_header(0x1122334455667788, 0x99AA, &mut buf).unwrap();

        assert_eq!(&buf[0..8], &0x1122334455667788u64.to_le_bytes());
        assert_eq!(&buf[8..10], &0x99AAu16.to_le_bytes());
    }

    #[test]
    fn test_encrypt_does_not_overwrite_extra_bytes() {
        let key = [9u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf = types::PacketBuffer::new();
        buf.copy_from_slice(b"hello\xAA\xBB\xCC\xDD").unwrap();

        let before = buf.as_slice().to_vec();

        let _ = crypt.encrypt(5, &mut buf).unwrap();

        // Solo los primeros 5 + 16 bytes pueden cambiar
        assert_eq!(&before[21..], &buf.as_slice()[21..]);
    }
    #[test]
    fn test_encrypt_produces_unique_nonces() {
        let key = [10u8; 32];
        let mut crypt = Crypt::new(&key);

        let mut buf1 = types::PacketBuffer::new();
        buf1.copy_from_slice(b"a").unwrap();
        let c1 = crypt.encrypt(1, &mut buf1).unwrap().to_vec();

        let mut buf2 = types::PacketBuffer::new();
        buf2.copy_from_slice(b"a").unwrap();
        let c2 = crypt.encrypt(1, &mut buf2).unwrap().to_vec();

        assert_ne!(c1, c2);
    }
}
