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

// Authors: Adolfo Gómez, dkmaster at dkmon dot com

use aes_gcm::{AeadInOut, Aes256Gcm, Nonce, Tag, aead::{Aead, AeadCore, KeyInit}};
use anyhow::Result;

use crate::log;

// Comms related
pub mod consts;
pub mod stream;
pub mod tunnel;
pub mod types;

// PQC related
pub mod kem;

pub struct Crypt {
    cipher: Aes256Gcm,
    seq: u64,
}

impl Crypt {
    pub fn new(key: &types::SharedSecret, seq: u64) -> Self {
        log::debug!("Creating Crypt with initial seq: {}", seq);
        let cipher = Aes256Gcm::new(key.as_ref().into());
        Crypt { cipher, seq }
    }

    /// Increments and returns the internal seq.
    /// Note: the encrypt method automatically calls this method to get a unique seq for each encryption.
    /// (so it is pre increment, that is, if seq is 0, first packet will have seq 1, and then seq will be 1 after the call also).
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
    /// The encryption is done inplace to avoid extra allocations.
    ///
    /// Note: length is the length of the plaintext data to encrypt.
    ///       also, the real data is written into buffer[2..], so first 2 bytes are free for channel id
    pub fn encrypt(
        &mut self,
        channel_id: u16,
        len: usize,
        buffer: &mut types::PacketBuffer,
    ) -> Result<usize> {
        types::PacketBuffer::ensure_capacity(len + consts::TAG_LENGTH)?;

        // Set the channel id in the buffer (first 2 bytes of the data part, after header)
        buffer.set_channel_id(channel_id);

        let data_with_channel_length = types::PacketBuffer::calc_data_with_channel_len(len)?;

        let seq = self.next_seq();
        buffer.set_seq(seq);
        buffer.set_length(data_with_channel_length + consts::TAG_LENGTH)?; // Write header with seq and length of encrypted data

        let mut nonce_arr = [0u8; 12];
        nonce_arr[..8].copy_from_slice(&seq.to_be_bytes());
        let nonce = Nonce::from(nonce_arr);
        let aad = seq.to_be_bytes();

        // Get pointer to data part of the buffer, where encryption will happen
        let data = buffer.data_with_channel_mut();

        // // Log before crypt
        // log::debug!(
        //     "ENC: seq {}, length {}: {:?}..{:?}, channel {}",
        //     seq,
        //     len,
        //     data[..std::cmp::min(8, len)].to_vec(),
        //     data[len.saturating_sub(8)..data_with_channel_length].to_vec(),
        //     channel_id
        // );

        let tag = self
            .cipher
            .encrypt_inout_detached(
                &nonce,
                &aad,
                (&mut data[..data_with_channel_length]).into(),
            )
            .map_err(|e| anyhow::anyhow!("encryption failure: {:?}", e))?;
        data[data_with_channel_length..data_with_channel_length + consts::TAG_LENGTH]
            .copy_from_slice(tag.as_slice());

        // Returns the FULL length of the encrypted packet (header + data + channel + tag)
        Ok(data_with_channel_length + consts::TAG_LENGTH)
    }

    /// Decrypts the given ciphertext using AES-GCM with a nonce derived from the provided seq.
    /// The nonce is constructed by taking the seq value and padding it to 12 bytes with
    /// zeros. The seq value is also used as associated data (AAD) to ensure integrity.
    /// Returns the decrypted plaintext on success, and the channel (first 2 bytes, little-endian u16).
    /// Note: length is the length on encrpypted data WITH the tag (so, as readed from the stream).
    pub fn decrypt(&mut self, buffer: &mut types::PacketBuffer) -> Result<()> {
        let seq = buffer.seq()?;
        if seq < self.current_seq() {
            return Err(anyhow::anyhow!(
                "replay attack detected: seq {} is less than current seq {}",
                seq,
                self.current_seq()
            ));
        }

        let length = buffer.length()?;
        if length < (consts::TAG_LENGTH + 2) {
            return Err(anyhow::anyhow!(
                "decryption failure: ciphertext too short: {} bytes",
                length
            ));
        }

        let len = length - consts::TAG_LENGTH;
        let chan_data_buffer = buffer.data_with_channel_mut();

        let mut nonce_arr = [0u8; 12];
        nonce_arr[..8].copy_from_slice(&seq.to_be_bytes());
        let nonce = Nonce::from(nonce_arr);
        let aad = seq.to_be_bytes();

        // Split ciphertext and tag. `Tag` is parameterised by the tag size (not by
        // the cipher); its default `U16` matches the tag size of `Aes256Gcm`.
        let (ciphertext, rest) = chan_data_buffer.split_at_mut(len);
        let tag: &Tag = (&rest[..consts::TAG_LENGTH])
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid tag length"))?;

        self.cipher
            .decrypt_inout_detached(&nonce, &aad, ciphertext.into(), tag)
            .map_err(|e| anyhow::anyhow!("decryption failure: {:?}", e))?;

        self.seq = seq + 1; // Update to last used seq + 1, so no replays are possible

        // Fix data length to remove ending tag, so only channel + data is left
        buffer.set_length(len)?;

        // let data = buffer.data_with_channel();
        // log::debug!(
        //     "DEC: seq {}, length {}: {:?}..{:?}, channel {}",
        //     seq,
        //     len,
        //     data[..std::cmp::min(8, len)].to_vec(),
        //     data[len.saturating_sub(8)..len].to_vec(),
        //     buffer.channel_id()
        // );

        Ok(())
    }

    /// Used to decrypt data that was encrypted with a key derived from the shared secret and the ticket id, without using the internal seq/nonce mechanism.
    pub fn simple_decrypt(
        key: &types::SharedSecret,
        nonce: &[u8; 12],
        data: &[u8],
    ) -> Result<Vec<u8>> {
        // aes-gcm 0.11 parameterises `Nonce` by the nonce size, not by the
        // cipher. `Aes256Gcm` is an alias of `AesGcm<Aes256, U12, U16>` and its
        // `NonceSize` is `U12`, so use the fully-qualified form.
        let nonce: &Nonce<<Aes256Gcm as AeadCore>::NonceSize> = &Nonce::from(*nonce);
        let cipher = Aes256Gcm::new(key.as_ref().into());
        cipher
            .decrypt(nonce, data)
            .map_err(|_| anyhow::format_err!("AES-256-GCM decryption failed"))
    }
}

#[cfg(test)]
mod tests {
    use crate::crypt::types::SharedSecret;

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
        log::setup_logging("debug", log::LogType::Test);

        let key = SharedSecret::new([7u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        let plaintext = b"16 length text!!";
        buf.set_data(plaintext).unwrap();

        // Packet buffer will contain the header + the crypted data + tag
        crypt.encrypt(1, plaintext.len(), &mut buf).unwrap();

        let mut buf2 = buf.clone(); // copy the buffer
        crypt.decrypt(&mut buf2).unwrap();

        assert_eq!(buf2.data(), plaintext);
        assert_eq!(buf2.channel_id(), 1);
    }

    #[test]
    fn test_sequence_increments() {
        let key = SharedSecret::new([1u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        assert_eq!(crypt.current_seq(), 0);
        assert_eq!(crypt.next_seq(), 1);
        assert_eq!(crypt.next_seq(), 2);
        assert_eq!(crypt.current_seq(), 2);
    }

    #[test]
    fn test_replay_fails() {
        let key = SharedSecret::new([2u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        buf.set_data(b"abc").unwrap();

        crypt.encrypt(2, 3, &mut buf).unwrap();
        assert_eq!(buf.seq().unwrap(), crypt.current_seq());
        assert_eq!(
            buf.length().unwrap(),
            types::PacketBuffer::calc_data_with_channel_len(3).unwrap() + consts::TAG_LENGTH
        );

        let mut buf2 = buf.clone(); // clone the buffer

        // First decrypt should work
        crypt.decrypt(&mut buf2).unwrap();

        // Second decrypt with the same seq should fail
        let mut buf3 = buf.clone(); // clone the original buffer again
        let result = crypt.decrypt(&mut buf3).unwrap_err();

        assert!(
            result.to_string().contains("replay attack detected"),
            "{}",
            result
        );
    }

    #[test]
    fn test_decrypt_fails_on_bad_tag() {
        log::setup_logging("debug", log::LogType::Test);

        let key = SharedSecret::new([3u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        buf.set_data(b"hola").unwrap();

        let length = crypt.encrypt(3, 4, &mut buf).unwrap();
        assert_eq!(buf.seq().unwrap(), crypt.current_seq());
        assert_eq!(buf.length().unwrap(), length); // Length of encrypted data + tag

        let mut corrupted = buf.clone();
        // flip some bits at the end of the tag ("data" length = channel (2 bytes) + data (4 bytes) = 6 + tag (16 bytes) = 22 bytes)
        let data_len = length - 2; // data points to data, not channel id, but length includes channel id length
        corrupted.data_mut()[data_len - 1] ^= 0xFF; // flip bit in the tag

        let err = crypt.decrypt(&mut corrupted).unwrap_err();

        assert!(err.to_string().contains("decryption failure"), "{}", err);
    }

    #[test]
    fn test_decrypt_fails_on_truncated_ciphertext() {
        let key = SharedSecret::new([4u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        buf.set_data(b"hola").unwrap();

        crypt.encrypt(3, 4, &mut buf).unwrap();

        let mut truncated = buf.clone();
        truncated.set_length(buf.length().unwrap() - 5).unwrap(); // Set length to 2, which is less than the required 2 (channel) + 16 (tag)

        let err = crypt.decrypt(&mut truncated).unwrap_err();

        assert!(
            err.to_string().contains("ciphertext too short"),
            "{:?}",
            err
        );
    }

    #[test]
    fn test_encrypt_does_not_overwrite_extra_bytes() {
        let key = SharedSecret::new([9u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        buf.full_buffer_mut().fill(0xAF); // Fill with known pattern

        let before = buf.data_with_channel().to_vec();

        // Channel 32, 4 bytes of data
        let _ = crypt.encrypt(32, 5, &mut buf).unwrap();

        let after = buf.data_with_channel();

        // Just first 5 +  2 + 16 bytes can be changed (channel + data + tag)
        assert_eq!(&before[23..], &after[23..]);
    }
    #[test]
    fn test_encrypt_produces_unique_nonces() {
        let key = SharedSecret::new([10u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf1 = types::PacketBuffer::new();
        buf1.set_data(b"a").unwrap();
        crypt.encrypt(1, 1, &mut buf1).unwrap();
        let c1 = buf1.buffer().unwrap().to_vec();

        let mut buf2 = types::PacketBuffer::new();
        buf2.set_data(b"a").unwrap();
        crypt.encrypt(1, 1, &mut buf2).unwrap();
        let c2 = buf2.buffer().unwrap().to_vec();
        assert_ne!(c1, c2);
    }

    // ── additional coverage ──────────────────────────────────────────────

    #[test]
    fn test_simple_decrypt_roundtrip() {
        // simple_decrypt is the inverse of the *payload* encryption used to
        // wrap the initial ticket info, which does NOT use the internal seq
        // machinery. We hand-craft a ciphertext with aes-gcm directly so we
        // can assert that the wrapper roundtrips and fails when tampered.
        use aes_gcm::aead::{Aead, KeyInit};

        let key = SharedSecret::new([0xABu8; 32]);
        let nonce_bytes = [0x11u8; 12];
        let plaintext = b"openuds-ticket-payload";

        let cipher = Aes256Gcm::new(key.as_ref().into());
        let ciphertext = cipher
            .encrypt(&aes_gcm::Nonce::from(nonce_bytes), plaintext.as_ref())
            .unwrap();

        let decrypted = Crypt::simple_decrypt(&key, &nonce_bytes, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);

        // Tampering with the tag must fail
        let mut bad = ciphertext.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        assert!(Crypt::simple_decrypt(&key, &nonce_bytes, &bad).is_err());

        // Wrong nonce must also fail (GCM is unauthenticated w/o the right key+nonce)
        let wrong_nonce = [0x22u8; 12];
        assert!(Crypt::simple_decrypt(&key, &wrong_nonce, &ciphertext).is_err());
    }

    #[test]
    fn test_decrypt_fails_on_wrong_aad() {
        // The seq is bound as associated data. If a different seq is forced
        // on a copy of the buffer, decryption must fail because the AAD check
        // does not match what was authenticated at encrypt time.
        let key = SharedSecret::new([0x55u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        let plaintext = b"aad-bound payload!!".to_vec();
        buf.set_data(&plaintext).unwrap();
        crypt.encrypt(7, plaintext.len(), &mut buf).unwrap();

        let mut tampered = buf.clone();
        let original_seq = tampered.seq().unwrap();
        tampered.set_seq(original_seq.wrapping_add(1));

        let err = crypt.decrypt(&mut tampered).unwrap_err();
        assert!(
            err.to_string().contains("decryption failure"),
            "{}",
            err
        );
    }

    #[test]
    fn test_decrypt_fails_on_wrong_nonce() {
        // The nonce is derived from the seq header, and GCM authenticates the
        // (key, nonce, aad) tuple. If an attacker rewrites the seq header so
        // the decrypt path builds a different nonce, GCM must reject the
        // ciphertext. Use a fresh receiver that has *not* seen any seqs yet,
        // so the replay check does not preempt the GCM check.
        let key = SharedSecret::new([0x66u8; 32]);
        let mut sender = Crypt::new(&key, 0);
        let mut receiver = Crypt::new(&key, 0);

        let mut pkt = types::PacketBuffer::new();
        pkt.set_data(b"same-payload").unwrap();
        sender.encrypt(1, 12, &mut pkt).unwrap();
        // pkt is now bound to seq=1 with the sender's seq.

        // Tamper the seq header to a never-seen value, so the receiver will
        // derive a *different* nonce when trying to decrypt. The replay
        // window is satisfied (99 > 0) and the AAD also changes, so the
        // GCM tag check must fail.
        pkt.set_seq(99);

        let err = receiver.decrypt(&mut pkt).unwrap_err();
        assert!(
            err.to_string().contains("decryption failure"),
            "{}",
            err
        );
    }

    #[test]
    fn test_decrypt_advances_seq() {
        // After a successful decrypt, the internal seq must advance to
        // seq+1 so that subsequent out-of-order / replayed packets get
        // rejected even if they look syntactically valid.
        let key = SharedSecret::new([0x77u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        buf.set_data(b"data").unwrap();
        crypt.encrypt(1, 4, &mut buf).unwrap();

        let after_encrypt = crypt.current_seq();
        crypt.decrypt(&mut buf).unwrap();
        assert_eq!(crypt.current_seq(), after_encrypt + 1);

        // A *new* encrypted packet that happens to land on the previous seq
        // (because we just re-encrypted with a fresh Crypt on the same
        // starting state) must be rejected as a replay.
        let mut other = Crypt::new(&key, 0);
        let mut replay = types::PacketBuffer::new();
        replay.set_data(b"new!").unwrap();
        other.encrypt(1, 4, &mut replay).unwrap();
        // `replay` now has seq=1, which is < our current seq=2.
        let err = crypt.decrypt(&mut replay).unwrap_err();
        assert!(
            err.to_string().contains("replay attack detected"),
            "{}",
            err
        );
    }

    #[test]
    fn test_encrypt_empty_payload() {
        // 0-byte payloads are a legitimate edge case (e.g. keep-alive). The
        // channel id is encrypted as part of the AAD-bound payload, and a
        // tag is still produced; the resulting length must be 2 (channel)
        // + 16 (tag) and roundtrip cleanly with the channel id restored.
        let key = SharedSecret::new([0x88u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut buf = types::PacketBuffer::new();
        let written = crypt.encrypt(42, 0, &mut buf).unwrap();

        assert_eq!(
            written,
            types::PacketBuffer::calc_data_with_channel_len(0).unwrap() + consts::TAG_LENGTH
        );

        let mut decrypted = buf.clone();
        crypt.decrypt(&mut decrypted).unwrap();
        assert_eq!(decrypted.data(), b"");
        // After decryption the channel id is restored in cleartext.
        assert_eq!(decrypted.channel_id(), 42);
    }

    #[test]
    fn test_encrypt_max_payload() {
        // Encrypt a payload at the maximum that the buffer can hold and make
        // sure it roundtrips without truncation. Uses CRYPT_PACKET_SIZE which
        // is the intended working max (well under MAX_PACKET_SIZE).
        let key = SharedSecret::new([0x99u8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let payload = vec![0xCDu8; consts::CRYPT_PACKET_SIZE];
        let mut buf = types::PacketBuffer::new();
        buf.set_data(&payload).unwrap();

        let written = crypt.encrypt(7, payload.len(), &mut buf).unwrap();
        assert_eq!(
            written,
            types::PacketBuffer::calc_data_with_channel_len(payload.len()).unwrap()
                + consts::TAG_LENGTH
        );

        let mut out = buf.clone();
        crypt.decrypt(&mut out).unwrap();
        assert_eq!(out.data(), payload.as_slice());
        assert_eq!(out.channel_id(), 7);
    }

    #[test]
    fn test_encrypt_decrypt_many_packets_in_order() {
        // Exercise the seq path with multiple back-to-back packets to make
        // sure nonce derivation, AAD, and seq advance all stay consistent.
        // `decrypt` advances the internal seq to `seq+1`, so after a full
        // roundtrip the seq must have advanced by 1 per packet.
        let key = SharedSecret::new([0xCCu8; 32]);
        let mut crypt = Crypt::new(&key, 0);

        let mut max_seq = 0u64;
        for i in 0u16..16 {
            let payload = format!("packet-{i:02}");
            let mut buf = types::PacketBuffer::new();
            buf.set_data(payload.as_bytes()).unwrap();
            crypt.encrypt(i, payload.len(), &mut buf).unwrap();
            let mut out = buf.clone();
            crypt.decrypt(&mut out).unwrap();
            assert_eq!(out.data(), payload.as_bytes());
            assert_eq!(out.channel_id(), i);
            max_seq = out.seq().unwrap();
        }

        // The last encrypted packet's seq is `initial + 16`. After the
        // matching decrypt the internal seq is set to that value + 1.
        assert_eq!(crypt.current_seq(), max_seq + 1);
    }
}
