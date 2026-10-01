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
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in the
//    documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its
//    contributors may be used to endorse or promote products derived from
//    this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Deterministic per-sequence rekeying for the tunnel legs.
//!
//! A session negotiates `k` (log2 of the rekey threshold, in frames) at the
//! `Open` handshake; the server owns the value and the launcher adopts it.
//! Frames are grouped into **epochs** by their sequence number:
//! `epoch(seq) = (seq - seq_base) >> k`. Epoch 0 uses the legacy tunnel keys
//! (the `openuds-ticket-crypt` / `openuds-ticket-crypt-udp` HKDF expands),
//! so `k = 0` (OFF) and the very first epoch of any session are byte-exactly
//! the pre-rekeying wire format. Epochs `n >= 1` expand the *same* HKDF
//! PRK (`Extract(salt = ticket, IKM = shared_secret)`) under the dedicated
//! info string below, with the transport, direction, `k` and epoch folded in
//! so no two (transport, dir, k, epoch) tuples can ever collide.
//!
//! There is no rekey message, no transition window, no "try both keys":
//! the key is a pure function of the frame's own sequence number, which is
//! carried in cleartext in the frame header (TCP) or datagram header (UDP).
//! The nonce/AAD keep binding the *global* per-direction seq, so nonce
//! uniqueness and the anti-replay machinery are untouched by epoch
//! boundaries. A late frame from an earlier epoch is simply re-derived from
//! its seq, and the [`RekeyState::cipher_for`] cache re-lands on that epoch's
//! cipher — the key is a pure function of `seq`, never of a clock or a
//! transition counter.

use std::sync::Arc;

use aes_gcm::{Aes256Gcm, aead::KeyInit};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::protocol::ticket;

use super::types::SharedSecret;

/// HKDF-Expand label for rekeying epochs. It MUST stay byte-identical on
/// both sides of the tunnel (server and launcher); the cross-repo
/// known-answer tests pin the resulting keys.
pub const REKEY_INFO_LABEL: &[u8] = b"openuds-tunnel-rekey";

/// Transport discriminator: the TCP tunnel leg.
pub const TRANSPORT_TCP: u8 = 0;
/// Transport discriminator: the UDP relay leg.
pub const TRANSPORT_UDP: u8 = 1;

/// Direction discriminator: frames/datagrams sent by the tunnel server.
pub const DIR_SERVER_TO_LAUNCHER: u8 = 0;
/// Direction discriminator: frames/datagrams sent by the launcher.
pub const DIR_LAUNCHER_TO_SERVER: u8 = 1;

/// Largest usable `k`. `k` is encoded in one byte on the wire and drives
/// `seq >> k` on a `u64` counter: values `>= 64` would be undefined shifts,
/// so a peer announcing one must be rejected outright (both the server
/// config parser and the `OpenResponse` parser enforce this bound).
pub const MAX_REKEY_LOG2: u8 = 63;

/// Default rekey threshold (`2^20` frames per epoch). Unset in the server
/// config resolves to this; justified against NIST SP 800-38D limits in the
/// design doc (`docs/plan/rekeying.md` §2).
pub const DEFAULT_REKEY_LOG2: u8 = 20;

/// The HKDF PRK of a session: `Extract(salt = ticket, IKM = shared_secret)`.
///
/// Epoch 0's material is expanded from it with the legacy labels (that is
/// exactly what `hkdf::Hkdf::new(salt, ikm)` does internally, so sharing the
/// PRK keeps epoch 0 byte-identical); epochs `n >= 1` expand it with
/// [`REKEY_INFO_LABEL`] and the per-(transport, dir, k, n) info string.
/// Zeroized on drop, redacted in logs.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionPrk([u8; 32]);

impl SessionPrk {
    pub fn derive(shared_secret: &SharedSecret, ticket: &ticket::Ticket) -> Self {
        let (prk, _) = Hkdf::<Sha256>::extract(Some(ticket.as_ref()), shared_secret.as_ref());
        // HMAC-SHA256 output: exactly 32 bytes.
        let mut out = [0u8; 32];
        out.copy_from_slice(&prk);
        SessionPrk(out)
    }

    /// Expand one epoch key:
    /// `K = HKDF-Expand(PRK, "openuds-tunnel-rekey" || transport_be8 ||
    ///     dir_be8 || k_be8 || epoch_be64, 32)`.
    pub fn expand_epoch_key(&self, transport: u8, dir: u8, k: u8, epoch: u64) -> SharedSecret {
        let hk = Hkdf::<Sha256>::from_prk(&self.0)
            .unwrap_or_else(|_| unreachable!("SessionPrk is exactly the SHA-256 output size"));
        // Equivalent to the fixed info concatenation of the design doc;
        // `expand_multi_info` feeds the same bytes to the HMAC without an
        // intermediate buffer.
        let mut okm = [0u8; 32];
        hk.expand_multi_info(
            &[
                REKEY_INFO_LABEL,
                &[transport],
                &[dir],
                &[k],
                &epoch.to_be_bytes(),
            ],
            &mut okm,
        )
        .unwrap_or_else(|_| unreachable!("32-byte output is always within the HKDF limit"));
        SharedSecret::new(okm)
    }
}

// Manual Debug: redacted, same posture as `SharedSecret`.
impl std::fmt::Debug for SessionPrk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SessionPrk({})",
            crate::log::redact_secret_bytes(&self.0)
        )
    }
}

/// Rekeying state shared by every crypt of one session *direction* (each
/// direction has its own `epoch0_key` and `dir` discriminator).
///
/// One `Arc<RekeyState>` is built per direction and handed to the crypts
/// built from the session's shared counters, so a stream replacement or a
/// recovery rebuilds crypts that agree on the epoch of any given `seq` —
/// the epoch is a pure function of `seq` and these immutable parameters,
/// never of per-crypt mutable state.
pub struct RekeyState {
    prk: Arc<SessionPrk>,
    transport: u8,
    dir: u8,
    /// Log2 of the rekey threshold. `0` means OFF (single epoch forever).
    k: u8,
    /// Epoch anchor of the counter feeding [`Self::epoch_of`]. `0` for the
    /// TCP leg (counters start near zero); `datagram::INITIAL_SEQ` for the
    /// UDP leg, whose seqs start at 2^63: the *first* datagram of a session
    /// must sit in epoch 0 no matter how high its absolute seq is.
    seq_base: u64,
    /// Pre-built cipher for epoch 0 / OFF (the legacy tunnel material for
    /// this direction). Epoch 0 never goes through the PRK expand, which
    /// keeps the pre-rekeying wire format byte-for-byte.
    epoch0_cipher: Arc<Aes256Gcm>,
}

impl RekeyState {
    pub fn new(
        prk: Arc<SessionPrk>,
        transport: u8,
        dir: u8,
        k: u8,
        seq_base: u64,
        epoch0_cipher: Arc<Aes256Gcm>,
    ) -> Self {
        assert!(k <= MAX_REKEY_LOG2, "rekey log2 above shift-safe bound");
        RekeyState {
            prk,
            transport,
            dir,
            k,
            seq_base,
            epoch0_cipher,
        }
    }

    /// Build a state for an epoch-0-only crypt with a given threshold:
    /// `k = 0` (OFF) collapses to a single epoch forever and the PRK is
    /// never expanded.
    pub fn epoch0_only(
        transport: u8,
        dir: u8,
        seq_base: u64,
        epoch0_cipher: Arc<Aes256Gcm>,
    ) -> Self {
        RekeyState {
            // The PRK is unused when k == 0; a zero PRK keeps the type
            // honest without deriving anything (never expanded).
            prk: Arc::new(SessionPrk([0u8; 32])),
            transport,
            dir,
            k: 0,
            seq_base,
            epoch0_cipher,
        }
    }

    /// Log2 threshold in force (0 = OFF).
    pub fn k(&self) -> u8 {
        self.k
    }

    /// Epoch of `seq` relative to `seq_base`. `k = 0` (OFF) is a single
    /// epoch forever, independent of `seq`. `saturating_sub` makes
    /// wire-crafted seqs below the base (impossible for an honest peer)
    /// land in epoch 0 instead of underflowing; they then fail the AEAD
    /// check under the epoch-0 key like any other forgery.
    pub fn epoch_of(&self, seq: u64) -> u64 {
        if self.k == 0 {
            return 0;
        }
        seq.saturating_sub(self.seq_base) >> self.k
    }

    /// Cipher for `epoch`. Epoch 0 (and OFF, whose `epoch_of` is constant 0
    /// by construction) returns the pre-built legacy cipher — an `Arc` clone,
    /// no re-derivation; every later epoch expands the session PRK once.
    pub fn cipher_for(&self, epoch: u64) -> Arc<Aes256Gcm> {
        if epoch == 0 {
            return self.epoch0_cipher.clone();
        }
        let key = self
            .prk
            .expand_epoch_key(self.transport, self.dir, self.k, epoch);
        Arc::new(Aes256Gcm::new(key.as_ref().into()))
    }
}

impl std::fmt::Debug for RekeyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RekeyState")
            .field("prk", &self.prk)
            .field("transport", &self.transport)
            .field("dir", &self.dir)
            .field("k", &self.k)
            .field("seq_base", &self.seq_base)
            .field("epoch0_cipher", &"<cached legacy cipher>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::AeadInOut;

    fn fixture() -> (SharedSecret, ticket::Ticket) {
        // Same fixed material as every other crypt KAT in the crate.
        (SharedSecret::new([1u8; 32]), [2u8; 48].into())
    }

    #[test]
    fn prk_matches_legacy_expand() {
        // The PRK must be the exact one the legacy `Hkdf::new(salt, ikm)`
        // uses internally: expanding the old label from the shared PRK
        // reproduces today's epoch-0 keys, which is what makes epoch 0
        // byte-identical to the pre-rekeying wire format.
        use super::super::tunnel::derive_tunnel_material;
        let (secret, ticket) = fixture();
        let prk = SessionPrk::derive(&secret, &ticket);
        let hk = Hkdf::<Sha256>::from_prk(&prk.0).unwrap();
        let mut legacy_prefix = [0u8; 32];
        hk.expand(b"openuds-ticket-crypt", &mut legacy_prefix)
            .unwrap();
        let material = derive_tunnel_material(&secret, &ticket).unwrap();
        // First 32 bytes of the 108-byte OKM are `key_payload`.
        assert_eq!(legacy_prefix, *material.key_payload.as_ref());
    }

    #[test]
    fn epoch_keys_are_domain_separated() {
        let (secret, ticket) = fixture();
        let prk = Arc::new(SessionPrk::derive(&secret, &ticket));
        let epoch0_key = SharedSecret::new([0xEEu8; 32]);
        let epoch0_cipher = Arc::new(Aes256Gcm::new(epoch0_key.as_ref().into()));
        let state = RekeyState::new(
            prk.clone(),
            TRANSPORT_TCP,
            DIR_SERVER_TO_LAUNCHER,
            8,
            0,
            epoch0_cipher.clone(),
        );
        let k_tcp_s2c_e1 = prk.expand_epoch_key(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 8, 1);
        let k_tcp_s2c_e2 = prk.expand_epoch_key(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 8, 2);
        let k_tcp_c2s_e1 = prk.expand_epoch_key(TRANSPORT_TCP, DIR_LAUNCHER_TO_SERVER, 8, 1);
        let k_udp_s2c_e1 = prk.expand_epoch_key(TRANSPORT_UDP, DIR_SERVER_TO_LAUNCHER, 8, 1);
        let k_tcp_s2c_e1_k9 = prk.expand_epoch_key(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 9, 1);

        // Every (transport, dir, k, epoch) tuple yields a distinct key.
        let all = [
            k_tcp_s2c_e1.clone(),
            k_tcp_s2c_e2,
            k_tcp_c2s_e1,
            k_udp_s2c_e1,
            k_tcp_s2c_e1_k9,
            epoch0_key.clone(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert!(all[i] != all[j], "keys {i} and {j} collide");
            }
        }

        // cipher_for must agree with a cipher built from the direct expand.
        let seq_e1 = (1u64 << 8) + 5;
        assert_eq!(state.epoch_of(seq_e1), 1);
        let cipher = state.cipher_for(state.epoch_of(seq_e1));
        let direct = Aes256Gcm::new(k_tcp_s2c_e1.as_ref().into());
        let mut buf_a = *b"test";
        let mut buf_b = *b"test";
        let nonce = aes_gcm::Nonce::from([0u8; 12]);
        let aad = seq_e1.to_be_bytes();
        let tag_a = cipher
            .encrypt_inout_detached(&nonce, &aad, (&mut buf_a[..]).into())
            .unwrap();
        let tag_b = direct
            .encrypt_inout_detached(&nonce, &aad, (&mut buf_b[..]).into())
            .unwrap();
        assert_eq!(buf_a, buf_b);
        assert_eq!(tag_a.as_slice(), tag_b.as_slice());
    }

    #[test]
    fn epoch_zero_cipher_matches_the_legacy_key() {
        let (secret, ticket) = fixture();
        let prk = Arc::new(SessionPrk::derive(&secret, &ticket));
        // A legacy direction key: the epoch-0 cipher must equal a cipher
        // built straight from it, so an epoch-0 frame is byte-identical to
        // the pre-rekeying wire format.
        let legacy = super::super::tunnel::derive_tunnel_material(&secret, &ticket)
            .unwrap()
            .key_send;
        let epoch0_cipher = Arc::new(Aes256Gcm::new(legacy.as_ref().into()));
        let state = RekeyState::new(
            prk,
            TRANSPORT_TCP,
            DIR_SERVER_TO_LAUNCHER,
            8,
            0,
            epoch0_cipher.clone(),
        );
        // cipher_for(0) hands back the SAME Arc (cheap clone, zero work).
        let returned = state.cipher_for(0);
        assert!(Arc::ptr_eq(&returned, &epoch0_cipher));
        // And that cipher equals one built directly from the legacy key.
        let direct = Aes256Gcm::new(legacy.as_ref().into());
        let mut buf_a = *b"test";
        let mut buf_b = *b"test";
        let nonce = aes_gcm::Nonce::from([0u8; 12]);
        let aad = 3u64.to_be_bytes();
        let tag_a = returned
            .encrypt_inout_detached(&nonce, &aad, (&mut buf_a[..]).into())
            .unwrap();
        let tag_b = direct
            .encrypt_inout_detached(&nonce, &aad, (&mut buf_b[..]).into())
            .unwrap();
        assert_eq!(buf_a, buf_b);
        assert_eq!(tag_a.as_slice(), tag_b.as_slice());
    }

    #[test]
    fn off_state_has_a_single_epoch() {
        let epoch0_cipher = Arc::new(Aes256Gcm::new(
            SharedSecret::new([0xAAu8; 32]).as_ref().into(),
        ));
        let off = RekeyState::epoch0_only(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 0, epoch0_cipher);
        assert_eq!(off.k(), 0);
        // Every seq is epoch 0 and uses the legacy key, even a huge one.
        assert_eq!(off.epoch_of(0), 0);
        assert_eq!(off.epoch_of(u64::MAX), 0);
    }

    #[test]
    fn udp_base_shifts_the_epoch_anchor() {
        use super::super::datagram::INITIAL_SEQ;
        let (secret, ticket) = fixture();
        let prk = Arc::new(SessionPrk::derive(&secret, &ticket));
        let epoch0_cipher = Arc::new(Aes256Gcm::new(
            SharedSecret::new([0xBBu8; 32]).as_ref().into(),
        ));
        let udp = RekeyState::new(
            prk,
            TRANSPORT_UDP,
            DIR_SERVER_TO_LAUNCHER,
            8,
            INITIAL_SEQ,
            epoch0_cipher,
        );
        // The first datagram (INITIAL_SEQ + 1) is epoch 0 regardless of its
        // absolute value.
        assert_eq!(udp.epoch_of(INITIAL_SEQ + 1), 0);
        assert_eq!(udp.epoch_of(INITIAL_SEQ + (1u64 << 8)), 1);
        // A crafted seq below the base saturates to epoch 0 (then fails the
        // AEAD check under the epoch-0 key; it never underflows).
        assert_eq!(udp.epoch_of(7), 0);
        assert_eq!(udp.epoch_of(0), 0);
        // A crafted seq at u64::MAX cannot panic the shift.
        assert_eq!(udp.epoch_of(u64::MAX), (u64::MAX - INITIAL_SEQ) >> 8);
    }
}
