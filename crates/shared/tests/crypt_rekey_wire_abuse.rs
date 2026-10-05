//! Wire-level abuse regressions for the per-epoch rekeying of the tunnel legs.
//!
//! Every test here targets an *abuse* shape a peer could try: forcing epoch
//! churn, presenting a frame under the wrong epoch key, sharing the sequence
//! counter between two crypts (live stream + recovery handshake), or sending a
//! datagram below the UDP epoch anchor. The correctness side of the contract
//! lives in `crypt_rekey_regressions.rs`.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::{AeadInOut, Aes256Gcm, aead::KeyInit};
use shared::crypt::{
    Crypt,
    datagram::{DatagramCrypt, INITIAL_SEQ, TOKEN_LENGTH},
    rekey::{DIR_LAUNCHER_TO_SERVER, RekeyState, SessionPrk, TRANSPORT_TCP, TRANSPORT_UDP},
    types::{PacketBuffer, SharedSecret},
};

const KEY: [u8; 32] = [0xEEu8; 32];

fn prk() -> Arc<SessionPrk> {
    Arc::new(SessionPrk::derive(
        &SharedSecret::new([1u8; 32]),
        &[2u8; 48].into(),
    ))
}

fn epoch0(key: [u8; 32]) -> Arc<Aes256Gcm> {
    Arc::new(Aes256Gcm::new(SharedSecret::new(key).as_ref().into()))
}

/// A launcher-shaped crypt: same key, same direction, same `k`, counter
/// seeded so the next `encrypt` goes out at `seq`.
fn sender_at(seq: u64, k: u8) -> Crypt {
    let rekey = Arc::new(RekeyState::new(
        prk(),
        TRANSPORT_TCP,
        DIR_LAUNCHER_TO_SERVER,
        k,
        0,
        epoch0(KEY),
    ));
    Crypt::with_rekey(Arc::new(AtomicU64::new(seq - 1)), rekey)
}

fn server_in(k: u8, seq: &Arc<AtomicU64>) -> Crypt {
    let rekey = Arc::new(RekeyState::new(
        prk(),
        TRANSPORT_TCP,
        DIR_LAUNCHER_TO_SERVER,
        k,
        0,
        epoch0(KEY),
    ));
    Crypt::with_rekey(seq.clone(), rekey)
}

fn frame(seq: u64, payload: &[u8], k: u8) -> PacketBuffer {
    let mut tx = sender_at(seq, k);
    let mut b = PacketBuffer::new();
    b.set_data(payload).unwrap();
    tx.encrypt(1, payload.len(), &mut b).unwrap();
    b
}

/// Two crypts sharing one direction counter (live stream + recovery
/// handshake) must agree on the epoch of any seq, and a frame already
/// consumed by one holder must be a replay for the other.
#[test]
fn cross_holder_epoch_agreement_and_replay() {
    let seq_in = Arc::new(AtomicU64::new(0));
    let mut a = server_in(20, &seq_in);
    let mut b = server_in(20, &seq_in);

    let mut f0 = frame(1, b"epoch-zero", 20);
    a.decrypt(&mut f0).unwrap();
    // the same frame handed to the other holder is a replay (shared counter)
    let mut f0b = frame(1, b"epoch-zero", 20);
    assert!(b.decrypt(&mut f0b).is_err());

    // a frame in a later epoch (seq = 3 << 20) decrypts through either holder
    let mut f3 = frame(3 << 20, b"epoch-three", 20);
    a.decrypt(&mut f3).unwrap();
    assert_eq!(seq_in.load(Ordering::SeqCst), (3 << 20) + 1);
    let mut f3b = frame(4 << 20, b"epoch-four", 20);
    b.decrypt(&mut f3b).unwrap();
    assert_eq!(seq_in.load(Ordering::SeqCst), (4 << 20) + 1);
}

/// A frame encrypted under the wrong epoch's key must never be accepted —
/// the epoch is part of the key, so a wrong-epoch frame is a plain AEAD
/// failure even when its tag was produced for a different epoch of the same
/// session.
#[test]
fn wrong_epoch_key_is_rejected() {
    // Encrypt with the epoch-0 key (a `k = 0` crypt) but place the frame at a
    // seq that belongs to epoch 3 of a `k = 20` receiver.
    let mut legacy = Crypt::new(&SharedSecret::new(KEY), (3 << 20) - 1);
    let mut buf = PacketBuffer::new();
    buf.set_data(b"wrong-epoch-key").unwrap();
    legacy.encrypt(1, 15, &mut buf).unwrap();
    assert_eq!(buf.seq().unwrap(), 3 << 20);

    let mut fresh = server_in(20, &Arc::new(AtomicU64::new(0)));
    assert!(
        fresh.decrypt(&mut buf).is_err(),
        "a frame encrypted under the epoch-0 key must not decrypt in epoch 3"
    );
}

/// Forged (no-key) frames that each claim a fresh epoch must not advance the
/// shared counter and must not deny the legitimate peer's next frame, even
/// though the receiving crypt did re-derive a key for each of them.
#[test]
fn forged_epoch_churn_does_not_advance_the_counter_or_deny_legit_frames() {
    let seq_in = Arc::new(AtomicU64::new(0));
    let mut server = server_in(20, &seq_in);

    for i in 1..=64u64 {
        let mut forged = PacketBuffer::from([0x41u8].as_slice());
        forged.set_seq(i << 20);
        forged.set_length(2 + 1 + 16).unwrap();
        forged.set_channel_id(1);
        assert!(
            server.decrypt(&mut forged).is_err(),
            "forged frame must fail"
        );
    }
    assert_eq!(
        seq_in.load(Ordering::SeqCst),
        0,
        "failed AEAD must not advance the shared counter"
    );

    let mut ok = frame(1, b"legit", 20);
    server
        .decrypt(&mut ok)
        .expect("the legitimate frame must still decrypt after the churn");
}

/// A peer that jumps the shared counter to a far epoch only trips the replay
/// floor for earlier seqs; the refusal is the replay check, never a wrong-key
/// acceptance.
#[test]
fn epoch_jump_earlier_seq_hits_the_replay_floor() {
    let seq_in = Arc::new(AtomicU64::new(0));
    let mut server = server_in(20, &seq_in);
    let mut jump = frame(9 << 20, b"jump", 20);
    server.decrypt(&mut jump).unwrap();
    assert_eq!(seq_in.load(Ordering::SeqCst), (9 << 20) + 1);

    let mut low = frame(1 << 20, b"low", 20);
    let err = server.decrypt(&mut low).expect_err("must be rejected");
    assert!(
        err.to_string().contains("replay"),
        "expected the replay floor, got: {err}"
    );
    assert_eq!(seq_in.load(Ordering::SeqCst), (9 << 20) + 1);
}

/// UDP: the epoch anchor is `INITIAL_SEQ`, so a datagram whose seq is *below*
/// the anchor saturates to epoch 0 (the legacy key). This documents that a
/// peer can keep the launcher→server direction on the epoch-0 key at will
/// (`epoch_of` saturates instead of rejecting), which is what defeats the
/// per-key frame bound on that leg; the replay window is unaffected.
#[test]
fn udp_datagram_below_the_epoch_anchor_is_epoch_zero() {
    let token: [u8; TOKEN_LENGTH] = [0x42u8; TOKEN_LENGTH];
    let state = || {
        Arc::new(RekeyState::new(
            prk(),
            TRANSPORT_UDP,
            DIR_LAUNCHER_TO_SERVER,
            20,
            INITIAL_SEQ,
            epoch0(KEY),
        ))
    };
    // The anchor itself is epoch 0 even though the seq is astronomically high.
    assert_eq!(state().epoch_of(INITIAL_SEQ + 1), 0);
    assert_eq!(state().epoch_of(INITIAL_SEQ + (1 << 20)), 1);
    // Below the anchor: saturates to epoch 0 (no underflow, no rejection).
    assert_eq!(state().epoch_of(1), 0);
    assert_eq!(state().epoch_of(0), 0);

    // A datagram below the anchor that carries a valid epoch-0 tag is
    // accepted by the receiver (window starts empty), i.e. the key is the
    // legacy one regardless of the absolute seq.
    let mut rx = DatagramCrypt::with_rekey(state());
    let cipher = epoch0(KEY);
    for s in [1u64, 2, 3, 1000] {
        let mut data = b"low-seq".to_vec();
        let mut aad = [0u8; TOKEN_LENGTH + 8];
        aad[..TOKEN_LENGTH].copy_from_slice(&token);
        aad[TOKEN_LENGTH..].copy_from_slice(&s.to_be_bytes());
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&s.to_be_bytes());
        let tag = cipher
            .encrypt_inout_detached(
                &aes_gcm::Nonce::from(nonce),
                &aad,
                data.as_mut_slice().into(),
            )
            .unwrap();
        let mut dg = Vec::new();
        dg.extend_from_slice(&token);
        dg.extend_from_slice(&s.to_be_bytes());
        dg.extend_from_slice(&data);
        dg.extend_from_slice(tag.as_slice());
        assert_eq!(
            rx.decrypt(&token, &dg).unwrap().as_deref(),
            Some(b"low-seq".as_slice()),
            "seq {s} below the epoch anchor must be accepted under the epoch-0 key"
        );
    }
}
