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

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use aes_gcm::{Aes256Gcm, aead::KeyInit};
use anyhow::Result;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::{log, protocol::ticket};

use super::{
    Crypt,
    datagram::DatagramCrypt,
    rekey::{
        DIR_LAUNCHER_TO_SERVER, DIR_SERVER_TO_LAUNCHER, RekeyState, SessionPrk, TRANSPORT_TCP,
        TRANSPORT_UDP,
    },
    types::SharedSecret,
};

#[derive(Debug)]
pub struct Material {
    pub key_payload: SharedSecret,
    pub key_receive: SharedSecret,
    pub key_send: SharedSecret,
    pub nonce_payload: [u8; 12],
}

pub fn derive_tunnel_material(
    shared_secret: &SharedSecret,
    ticket: &ticket::Ticket,
) -> Result<Material> {
    log::debug!(
        "Deriving tunnel material with shared_secret: {:?} and ticket: {:?}",
        shared_secret,
        ticket
    );

    // HKDF-Extract + Expand with SHA-256
    let hk = Hkdf::<Sha256>::new(Some(ticket.as_ref()), shared_secret.as_ref());

    let mut okm = [0u8; 108];
    hk.expand(b"openuds-ticket-crypt", &mut okm)
        .map_err(|_| anyhow::format_err!("HKDF expand failed"))?;

    let mut key_payload = [0u8; 32];
    let mut key_send = [0u8; 32];
    let mut key_receive = [0u8; 32];
    let mut nonce_payload = [0u8; 12];

    // Skipping first 32 bytes being not used here
    key_payload.copy_from_slice(&okm[0..32]);
    key_send.copy_from_slice(&okm[32..64]);
    key_receive.copy_from_slice(&okm[64..96]);
    nonce_payload.copy_from_slice(&okm[96..108]);

    // Note: The key_send is the key used by sender, so we use this key for decrypting recived data (inbound)
    // and key_receive is the key used by receiver, so we use this key for encrypting data to send (outbound)
    let material = Material {
        key_payload: key_payload.into(),
        key_receive: key_send.into(),
        key_send: key_receive.into(),
        nonce_payload,
    };

    // Zeroize the HKDF output before it goes out of scope. The
    // `key_*` arrays have already been moved into the `SharedSecret`
    // fields of `material`, so they are gone from the stack. The
    // `SharedSecret` fields are `ZeroizeOnDrop` and will be wiped
    // when `material` is eventually consumed by `get_tunnel_crypts`
    // and dropped. `okm` itself still holds every derived byte
    // (3 key segments + nonce) and must be wiped explicitly.
    okm.zeroize();

    Ok(material)
}

/// The rekeying states of one session leg, from the SERVER point of view:
/// `inbound` decrypts launcher→server traffic, `outbound` encrypts
/// server→launcher traffic. Both share the session PRK and the negotiated
/// `k`; they differ only in direction (the epoch-key domain separator).
///
/// Built once at handshake time and stored on the `Session`, so every crypt
/// the session hands out afterwards (live stream, stream replacement,
/// recovery handshake) rekeys under exactly the same parameters — and a
/// `Recover` never re-reads the config or renegotiates `k`.
pub struct TunnelRekeys {
    pub inbound: Arc<RekeyState>,
    pub outbound: Arc<RekeyState>,
}

impl TunnelRekeys {
    /// Build the pair from a session's shared PRK and already-derived
    /// epoch-0 material (the server `Session` keeps the PRK for its whole
    /// lifetime and re-builds this cheap struct per crypt pair, so the
    /// HKDF extract runs once per session, never per stream).
    pub fn from_parts(prk: Arc<SessionPrk>, material: &Material, k: u8) -> Self {
        TunnelRekeys::with_prk(prk, material, k)
    }

    fn from_material(
        material: &Material,
        shared_secret: &SharedSecret,
        ticket: &ticket::Ticket,
        k: u8,
    ) -> Self {
        let prk = Arc::new(SessionPrk::derive(shared_secret, ticket));
        TunnelRekeys::with_prk(prk, material, k)
    }

    fn with_prk(prk: Arc<SessionPrk>, material: &Material, k: u8) -> Self {
        TunnelRekeys {
            inbound: Arc::new(RekeyState::new(
                prk.clone(),
                TRANSPORT_TCP,
                DIR_LAUNCHER_TO_SERVER,
                k,
                0,
                Arc::new(Aes256Gcm::new(material.key_receive.as_ref().into())),
            )),
            outbound: Arc::new(RekeyState::new(
                prk,
                TRANSPORT_TCP,
                DIR_SERVER_TO_LAUNCHER,
                k,
                0,
                Arc::new(Aes256Gcm::new(material.key_send.as_ref().into())),
            )),
        }
    }
}

/// Returns (inbound, outbound) crypts sharing the provided sequence counters.
/// inbound: for reading from the tunnel (decrypting)
/// outbound: for writing to the tunnel (encrypting)
///
/// The counters are `Arc<AtomicU64>` so a caller (the server session) can keep
/// one authoritative counter per direction and hand it to *every* crypt it
/// builds — initial stream, recovery handshake, live stream — guaranteeing no
/// `(key, seq)` nonce pair is ever used twice, even while two streams overlap
/// for a short window.
///
/// `k` is the session's rekey threshold (log2 of frames per key epoch;
/// 0 = OFF, byte-identical to the legacy single-key construction).
/// # Arguments
/// * `shared_secret` - Shared secret used for deriving the keys
/// * `ticket` - Ticket used for deriving the keys
/// * `seq_in` - Sequence counter (shared) for the inbound crypt
/// * `seq_out` - Sequence counter (shared) for the outbound crypt
/// * `k` - Rekey threshold exponent negotiated for the session
pub fn get_tunnel_crypts(
    shared_secret: &SharedSecret,
    ticket: &ticket::Ticket,
    seq_in: Arc<AtomicU64>,
    seq_out: Arc<AtomicU64>,
    k: u8,
) -> Result<(Crypt, Crypt)> {
    let material = derive_tunnel_material(shared_secret, ticket)?;
    log::debug!(
        "Derived tunnel material: key_receive={:?}, key_send={:?}",
        material.key_receive,
        material.key_send
    );

    let rekeys = TunnelRekeys::from_material(&material, shared_secret, ticket, k);
    let inbound = Crypt::with_rekey(seq_in, rekeys.inbound);
    let outbound = Crypt::with_rekey(seq_out, rekeys.outbound);

    Ok((inbound, outbound))
}

/// Returns (inbound, outbound) UDP datagram crypts, from the SERVER point of
/// view: inbound decrypts datagrams from the launcher (key_client_to_server),
/// outbound encrypts datagrams towards it (key_server_to_client). The launcher
/// must use the same two keys swapped (its send key is our inbound key).
///
/// Keys are derived with a dedicated HKDF label so they are independent from
/// the TCP leg keys even though both come from the same ticket shared secret
/// (domain separation). Sequence numbers also live in their own space: the
/// UDP leg does not interact with `Session` seqs at all.
///
/// `k` is the same session-wide rekey threshold as the TCP leg (0 = OFF,
/// byte-identical to the legacy single-key construction). UDP seqs are
/// anchored at `INITIAL_SEQ`, so the epoch counting starts at the session's
/// first datagram whatever the absolute counter value. The replay window is
/// orthogonal to rekeying: it bounds *which* seqs are accepted; the epoch
/// only decides *which key* decrypts them.
pub fn get_udp_crypts(
    shared_secret: &SharedSecret,
    ticket: &ticket::Ticket,
    k: u8,
) -> Result<(DatagramCrypt, DatagramCrypt)> {
    let hk = Hkdf::<Sha256>::new(Some(ticket.as_ref()), shared_secret.as_ref());

    let mut okm = [0u8; 64];
    hk.expand(b"openuds-ticket-crypt-udp", &mut okm)
        .map_err(|_| anyhow::format_err!("HKDF expand failed"))?;

    let mut key_client_to_server = [0u8; 32];
    let mut key_server_to_client = [0u8; 32];
    key_client_to_server.copy_from_slice(&okm[0..32]);
    key_server_to_client.copy_from_slice(&okm[32..64]);
    okm.zeroize();

    let prk = Arc::new(SessionPrk::derive(shared_secret, ticket));
    let c2s: SharedSecret = key_client_to_server.into();
    let s2c: SharedSecret = key_server_to_client.into();
    let inbound = DatagramCrypt::with_rekey(Arc::new(RekeyState::new(
        prk.clone(),
        TRANSPORT_UDP,
        DIR_LAUNCHER_TO_SERVER,
        k,
        DatagramCrypt::INITIAL_SEQ_ANCHOR,
        Arc::new(Aes256Gcm::new(c2s.as_ref().into())),
    )));
    let outbound = DatagramCrypt::with_rekey(Arc::new(RekeyState::new(
        prk,
        TRANSPORT_UDP,
        DIR_SERVER_TO_LAUNCHER,
        k,
        DatagramCrypt::INITIAL_SEQ_ANCHOR,
        Arc::new(Aes256Gcm::new(s2c.as_ref().into())),
    )));

    Ok((inbound, outbound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypt::datagram;

    #[test]
    fn test_derive_tunnel_material() {
        let shared_secret = SharedSecret::new([1u8; 32]);
        let ticket: ticket::Ticket = [2u8; 48].into();

        let material = derive_tunnel_material(&shared_secret, &ticket).unwrap();

        // Verify derived keys, known values
        assert_eq!(
            *material.key_receive.as_ref(),
            [
                165, 213, 31, 20, 62, 238, 14, 209, 50, 193, 226, 239, 216, 45, 76, 37, 101, 11,
                173, 113, 185, 254, 51, 7, 50, 39, 232, 253, 55, 12, 21, 156
            ]
        );
        assert_eq!(
            *material.key_send.as_ref(),
            [
                30, 79, 83, 235, 53, 71, 186, 71, 34, 250, 3, 51, 222, 193, 90, 208, 48, 112, 207,
                208, 219, 166, 191, 4, 208, 106, 159, 121, 221, 115, 30, 174
            ]
        );
    }

    #[test]
    fn test_get_tunnel_crypts() {
        let shared_secret = SharedSecret::new([1u8; 32]);
        let ticket: ticket::Ticket = [2u8; 48].into();

        let seq_in = Arc::new(AtomicU64::new(0));
        let seq_out = Arc::new(AtomicU64::new(0));
        let (inbound, outbound) =
            get_tunnel_crypts(&shared_secret, &ticket, seq_in.clone(), seq_out.clone(), 0).unwrap();

        assert_eq!(inbound.current_seq(), 0);
        assert_eq!(outbound.current_seq(), 0);

        // The crypts hold the *same* atomics the caller passed in: advancing
        // one through the crypt is visible through the caller's handle. This
        // is how the server session keeps one authoritative counter per
        // direction across stream replacements.
        let mut inbound = inbound;
        let mut outbound = outbound;
        assert_eq!(outbound.next_seq(), 1);
        assert_eq!(seq_out.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(inbound.next_seq(), 1);
        assert_eq!(seq_in.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Known-answer test for the UDP leg key derivation. The exact same
    /// expected values live in the launcher's crypt crate, so a drift in
    /// either implementation breaks a build. Expected values produced by an
    /// independent HKDF-SHA256 implementation (RFC 5869).
    #[test]
    fn test_get_udp_crypts_known_answer() {
        let shared_secret = SharedSecret::new([1u8; 32]);
        let ticket: ticket::Ticket = [2u8; 48].into();

        let (mut inbound, mut outbound) = get_udp_crypts(&shared_secret, &ticket, 0).unwrap();

        // Roundtrip must work with the (server-perspective) key pair:
        // outbound encrypts, inbound... cannot decrypt (different keys), so
        // verify by deriving the launcher's mirror pair manually.
        let token = [0x42u8; datagram::TOKEN_LENGTH];
        let datagram = outbound.encrypt(&token, b"kat").unwrap();

        // The launcher-side inbound crypt uses the server's outbound key
        // (s2c). Assert via known-answer: encrypting with a crypt built on
        // the expected s2c key must produce the exact same datagram.
        let expected_s2c: [u8; 32] = [
            115, 122, 103, 8, 221, 26, 166, 141, 102, 141, 74, 208, 99, 240, 91, 76, 233, 111, 200,
            0, 152, 79, 177, 241, 178, 56, 195, 87, 176, 182, 35, 9,
        ];
        let mut reference = DatagramCrypt::new(&SharedSecret::new(expected_s2c));
        assert_eq!(reference.encrypt(&token, b"kat").unwrap(), datagram);

        // Same for the c2s (inbound) direction.
        let expected_c2s: [u8; 32] = [
            165, 215, 81, 8, 62, 101, 176, 192, 153, 20, 87, 9, 192, 41, 1, 145, 120, 68, 37, 43,
            6, 56, 160, 235, 231, 173, 137, 157, 132, 240, 48, 25,
        ];
        let mut reference = DatagramCrypt::new(&SharedSecret::new(expected_c2s));
        let datagram = reference.encrypt(&token, b"kat").unwrap();
        assert_eq!(
            inbound.decrypt(&token, &datagram).unwrap().as_deref(),
            Some(b"kat".as_slice())
        );
    }

    /// Known-answer test for the rekeying epoch keys (`k = 8`, epochs 1 and
    /// 2, TCP + UDP, both directions). The expected bytes were produced by
    /// an independent HKDF-SHA256 implementation (RFC 5869, Python `hmac`),
    /// and the *exact same* literals live in the launcher's crypt crate:
    /// any drift in the label, the info-string layout, the PRK salt order,
    /// or the epoch encoding breaks both repos' builds in lockstep.
    #[test]
    fn rekey_epoch_keys_known_answer() {
        let shared_secret = SharedSecret::new([1u8; 32]);
        let ticket: ticket::Ticket = [2u8; 48].into();
        let prk = SessionPrk::derive(&shared_secret, &ticket);

        let expected: [(&str, u8, u8, u64, [u8; 32]); 8] = [
            (
                "tcp_s2c",
                TRANSPORT_TCP,
                DIR_SERVER_TO_LAUNCHER,
                1,
                [
                    79, 111, 99, 128, 70, 211, 132, 77, 192, 222, 84, 130, 154, 162, 88, 239, 203,
                    189, 0, 141, 50, 230, 46, 252, 14, 157, 248, 119, 251, 228, 250, 149,
                ],
            ),
            (
                "tcp_s2c",
                TRANSPORT_TCP,
                DIR_SERVER_TO_LAUNCHER,
                2,
                [
                    240, 203, 239, 99, 128, 62, 61, 152, 29, 166, 210, 244, 174, 249, 114, 211,
                    189, 155, 103, 215, 126, 50, 32, 137, 156, 176, 79, 156, 32, 232, 36, 179,
                ],
            ),
            (
                "tcp_c2s",
                TRANSPORT_TCP,
                DIR_LAUNCHER_TO_SERVER,
                1,
                [
                    83, 223, 100, 234, 119, 75, 131, 101, 238, 103, 165, 90, 109, 28, 239, 96, 190,
                    48, 110, 1, 127, 85, 205, 106, 215, 236, 116, 195, 23, 77, 172, 61,
                ],
            ),
            (
                "tcp_c2s",
                TRANSPORT_TCP,
                DIR_LAUNCHER_TO_SERVER,
                2,
                [
                    205, 241, 254, 207, 228, 95, 175, 11, 181, 221, 231, 79, 183, 50, 109, 234,
                    106, 28, 90, 49, 76, 165, 139, 58, 181, 31, 2, 92, 60, 210, 192, 113,
                ],
            ),
            (
                "udp_s2c",
                TRANSPORT_UDP,
                DIR_SERVER_TO_LAUNCHER,
                1,
                [
                    28, 26, 246, 136, 138, 84, 30, 255, 242, 110, 32, 59, 146, 142, 37, 79, 121,
                    83, 219, 253, 179, 62, 234, 106, 144, 133, 168, 144, 27, 78, 19, 161,
                ],
            ),
            (
                "udp_s2c",
                TRANSPORT_UDP,
                DIR_SERVER_TO_LAUNCHER,
                2,
                [
                    248, 168, 61, 52, 210, 101, 210, 130, 190, 48, 84, 54, 11, 101, 33, 228, 211,
                    75, 76, 7, 79, 94, 91, 121, 229, 81, 85, 137, 235, 71, 56, 183,
                ],
            ),
            (
                "udp_c2s",
                TRANSPORT_UDP,
                DIR_LAUNCHER_TO_SERVER,
                1,
                [
                    117, 128, 118, 150, 159, 228, 81, 193, 20, 97, 170, 238, 129, 243, 239, 105,
                    48, 165, 37, 187, 170, 93, 252, 195, 170, 71, 92, 163, 232, 60, 191, 41,
                ],
            ),
            (
                "udp_c2s",
                TRANSPORT_UDP,
                DIR_LAUNCHER_TO_SERVER,
                2,
                [
                    244, 25, 14, 167, 163, 98, 129, 218, 50, 128, 134, 42, 190, 8, 104, 26, 46,
                    156, 106, 75, 94, 105, 230, 156, 160, 211, 187, 35, 77, 17, 30, 126,
                ],
            ),
        ];

        for (name, transport, dir, epoch, expected_key) in expected {
            let key = prk.expand_epoch_key(transport, dir, 8, epoch);
            assert_eq!(
                key.as_ref().to_vec(),
                expected_key.to_vec(),
                "rekey epoch key {name} epoch {epoch}"
            );
        }
    }

    /// The UDP keys must differ from the TCP leg keys (domain separation).
    #[test]
    fn test_udp_keys_differ_from_tcp_keys() {
        let shared_secret = SharedSecret::new([1u8; 32]);
        let ticket: ticket::Ticket = [2u8; 48].into();

        let material = derive_tunnel_material(&shared_secret, &ticket).unwrap();
        let (udp_in, mut udp_out) = get_udp_crypts(&shared_secret, &ticket, 0).unwrap();
        let token = [7u8; datagram::TOKEN_LENGTH];

        // A datagram encrypted with the UDP outbound key must not verify
        // under a crypt built with any of the TCP leg keys.
        let datagram = udp_out.encrypt(&token, b"x").unwrap();
        for key in [
            &material.key_send,
            &material.key_receive,
            &material.key_payload,
        ] {
            let mut wrong = DatagramCrypt::new(key);
            assert!(wrong.decrypt(&token, &datagram).is_err());
        }
        drop(udp_in);
    }

    // This will not compile, as ticket length is enforced by type
    // #[test]
    // fn test_invalid_ticket_length() {
    //     let shared_secret = [1u8; 32];
    //     let ticket_id = [2u8; 16]; // Too short

    //     let result = get_tunnel_crypts(&shared_secret, &ticket_id);
    //     assert!(result.is_err());
    // }
}
