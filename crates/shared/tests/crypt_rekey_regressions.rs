//! Per-epoch rekeying: crypto-correctness regression probes.
//!
//! Origin: round-5 adversarial harness (591746e0), kept as permanent
//! regressions. Additive only; no product code is modified.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aes_gcm::{Aes256Gcm, aead::KeyInit};
use shared::crypt::{
    Crypt,
    datagram::{DatagramCrypt, INITIAL_SEQ, TOKEN_LENGTH},
    rekey::{
        DIR_LAUNCHER_TO_SERVER, DIR_SERVER_TO_LAUNCHER, MAX_REKEY_LOG2, RekeyState, SessionPrk,
        TRANSPORT_TCP, TRANSPORT_UDP,
    },
    tunnel::{derive_tunnel_material, get_tunnel_crypts, get_udp_crypts},
    types::{PacketBuffer, SharedSecret},
};
use shared::protocol::ticket::Ticket;

const TOKEN: [u8; TOKEN_LENGTH] = [0x42; TOKEN_LENGTH];
/// Fixed epoch-0 cipher material so a sender/receiver pair built through these
/// helpers agrees on the legacy key (the PRK-derived epoch keys are
/// deterministic anyway).
const EPOCH0_KEY: [u8; 32] = [0x33; 32];

fn secret() -> SharedSecret {
    SharedSecret::new([1u8; 32])
}
fn ticket() -> Ticket {
    [2u8; 48].into()
}
fn prk() -> Arc<SessionPrk> {
    Arc::new(SessionPrk::derive(&secret(), &ticket()))
}

fn tcp_state(k: u8, dir: u8) -> Arc<RekeyState> {
    Arc::new(RekeyState::new(
        prk(),
        TRANSPORT_TCP,
        dir,
        k,
        0,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    ))
}

fn udp_state(k: u8, dir: u8) -> Arc<RekeyState> {
    Arc::new(RekeyState::new(
        prk(),
        TRANSPORT_UDP,
        dir,
        k,
        INITIAL_SEQ,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    ))
}

fn frame() -> PacketBuffer {
    let mut b = PacketBuffer::new();
    b.set_data(b"rekey-probe").unwrap();
    b
}

fn forged(seq: u64) -> PacketBuffer {
    let mut b = PacketBuffer::new();
    b.set_seq(seq);
    b.set_length(2 + 13 + 16).unwrap();
    b.set_channel_id(1);
    b
}

// ---------------------------------------------------------------------------
// H1/H3 — epoch boundary exactness + k = 0 collapse
// ---------------------------------------------------------------------------
#[test]
fn p1_epoch_boundaries_are_exact() {
    for k in 1..=8u8 {
        let st = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
        assert_eq!(st.epoch_of(0), 0, "k={k} seq 0");
        assert_eq!(st.epoch_of(1), 0, "k={k} seq 1");
        let p = 1u64 << k;
        assert_eq!(st.epoch_of(p - 1), 0, "k={k} last of epoch 0");
        assert_eq!(st.epoch_of(p), 1, "k={k} first of epoch 1");
        assert_eq!(st.epoch_of(p + 1), 1, "k={k}");
        assert_eq!(st.epoch_of(2 * p - 1), 1, "k={k} last of epoch 1");
        assert_eq!(st.epoch_of(2 * p), 2, "k={k} first of epoch 2");
        assert_eq!(st.epoch_of(u64::MAX), u64::MAX >> k, "k={k} u64::MAX");

        let u = udp_state(k, DIR_SERVER_TO_LAUNCHER);
        assert_eq!(u.epoch_of(INITIAL_SEQ), 0, "udp k={k} anchor");
        assert_eq!(u.epoch_of(INITIAL_SEQ + 1), 0, "udp k={k} first datagram");
        assert_eq!(u.epoch_of(INITIAL_SEQ + p - 1), 0, "udp k={k} last epoch 0");
        assert_eq!(u.epoch_of(INITIAL_SEQ + p), 1, "udp k={k} first epoch 1");
        assert_eq!(
            u.epoch_of(INITIAL_SEQ + 2 * p),
            2,
            "udp k={k} first epoch 2"
        );
        assert_eq!(u.epoch_of(0), 0, "udp k={k} seq 0");
        assert_eq!(u.epoch_of(1), 0, "udp k={k} seq 1");
        assert_eq!(u.epoch_of(INITIAL_SEQ - 1), 0, "udp k={k} base-1");
    }

    let off = Arc::new(RekeyState::epoch0_only(
        TRANSPORT_TCP,
        DIR_SERVER_TO_LAUNCHER,
        0,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    ));
    for s in [0u64, 1, 1 << 20, 1 << 40, u64::MAX] {
        assert_eq!(off.epoch_of(s), 0, "OFF seq {s}");
    }
    let off_udp = Arc::new(RekeyState::epoch0_only(
        TRANSPORT_UDP,
        DIR_SERVER_TO_LAUNCHER,
        INITIAL_SEQ,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    ));
    for s in [0u64, 1, INITIAL_SEQ, u64::MAX] {
        assert_eq!(off_udp.epoch_of(s), 0, "OFF udp seq {s}");
    }
}

// ---------------------------------------------------------------------------
// H1 — (key, nonce) binding: a frame authenticates only under the key of its
// OWN seq's epoch. The nonce is the seq (injective), so this is the whole
// uniqueness argument.
// ---------------------------------------------------------------------------
#[test]
fn p2_key_is_bound_to_the_frames_own_seq() {
    let k = 3u8;
    let state = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
    let mut tx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());

    for _ in 1..=40u64 {
        let mut b = frame();
        tx.encrypt(1, 11, &mut b).unwrap();
        let seq = b.seq().unwrap();
        let epoch = seq >> k;

        // (a) control: the matching epoch authenticates
        let mut ok = Crypt::with_rekey(Arc::new(AtomicU64::new(seq - 1)), state.clone());
        let mut copy = b.clone();
        ok.decrypt(&mut copy)
            .unwrap_or_else(|e| panic!("control frame seq={seq} epoch={epoch}: {e}"));

        // (b) same epoch, different seq -> different nonce/AAD -> must fail
        let mut same_epoch = b.clone();
        same_epoch.set_seq(seq + 1);
        let mut rx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
        assert!(
            rx.decrypt(&mut same_epoch).is_err(),
            "seq={seq}: frame verified at another nonce of the same epoch"
        );

        // (c) different epoch -> different key and nonce -> must fail
        let mut other_epoch = b.clone();
        other_epoch.set_seq(seq + (1 << k));
        let mut rx2 = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
        assert!(
            rx2.decrypt(&mut other_epoch).is_err(),
            "seq={seq}: frame verified under another epoch's key"
        );
    }
}

// ---------------------------------------------------------------------------
// H2/H3 — epoch 0 and k = 0 are byte-identical to the legacy construction
// ---------------------------------------------------------------------------
#[test]
fn p3_epoch0_and_off_are_byte_identical_to_legacy_tcp() {
    let key = SharedSecret::new([0x44; 32]);

    let mut legacy = Crypt::with_counter(&key, Arc::new(AtomicU64::new(0)));
    let off = Arc::new(RekeyState::epoch0_only(
        TRANSPORT_TCP,
        DIR_SERVER_TO_LAUNCHER,
        0,
        Arc::new(Aes256Gcm::new(key.as_ref().into())),
    ));
    let mut off_crypt = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), off);
    for i in 0..600u16 {
        let mut a = frame();
        let mut b = frame();
        legacy.encrypt(i, 11, &mut a).unwrap();
        off_crypt.encrypt(i, 11, &mut b).unwrap();
        assert_eq!(a.buffer().unwrap(), b.buffer().unwrap(), "OFF frame {i}");
    }

    let mut legacy2 = Crypt::with_counter(&key, Arc::new(AtomicU64::new(0)));
    let rekeyed = Arc::new(RekeyState::new(
        prk(),
        TRANSPORT_TCP,
        DIR_SERVER_TO_LAUNCHER,
        8,
        0,
        Arc::new(Aes256Gcm::new(key.as_ref().into())),
    ));
    let mut rk = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), rekeyed);
    for i in 1..=200u16 {
        let mut a = frame();
        let mut b = frame();
        legacy2.encrypt(i, 11, &mut a).unwrap();
        rk.encrypt(i, 11, &mut b).unwrap();
        assert_eq!(a.seq().unwrap(), b.seq().unwrap());
        assert_eq!(
            a.buffer().unwrap(),
            b.buffer().unwrap(),
            "k=8 epoch-0 frame {i} must equal the legacy wire bytes"
        );
    }
}

#[test]
fn p4_udp_epoch0_is_byte_identical_to_legacy() {
    let (_, mut off) = get_udp_crypts(&secret(), &ticket(), 0).unwrap();
    let (_, mut rk) = get_udp_crypts(&secret(), &ticket(), 8).unwrap();
    for i in 0..200 {
        let a = off.encrypt(&TOKEN, b"payload").unwrap();
        let b = rk.encrypt(&TOKEN, b"payload").unwrap();
        assert_eq!(
            a, b,
            "UDP datagram {i}: k=8 epoch 0 must equal the legacy bytes"
        );
    }
}

// ---------------------------------------------------------------------------
// H5 — hostile seqs never panic / never underflow
// ---------------------------------------------------------------------------
#[test]
fn p5_hostile_seqs_do_not_panic() {
    for seq in [0u64, 1, u64::MAX - 1, u64::MAX] {
        let out = catch_unwind(AssertUnwindSafe(|| {
            let mut rx = Crypt::with_rekey(
                Arc::new(AtomicU64::new(0)),
                tcp_state(4, DIR_LAUNCHER_TO_SERVER),
            );
            let mut b = forged(seq);
            rx.decrypt(&mut b).is_ok()
        }));
        assert!(out.is_ok(), "TCP decrypt panicked on seq={seq}");
    }

    let mut rx = Crypt::with_rekey(
        Arc::new(AtomicU64::new(u64::MAX)),
        tcp_state(4, DIR_LAUNCHER_TO_SERVER),
    );
    let err = rx.decrypt(&mut forged(u64::MAX)).unwrap_err().to_string();
    assert!(err.contains("u64::MAX"), "{err}");

    for seq in [0u64, 1, INITIAL_SEQ - 1, INITIAL_SEQ, u64::MAX] {
        let out = catch_unwind(AssertUnwindSafe(|| {
            let mut rx = DatagramCrypt::with_rekey(udp_state(4, DIR_LAUNCHER_TO_SERVER));
            let mut d = vec![0u8; TOKEN_LENGTH + 8 + 1 + 16];
            d[..TOKEN_LENGTH].copy_from_slice(&TOKEN);
            d[TOKEN_LENGTH..TOKEN_LENGTH + 8].copy_from_slice(&seq.to_be_bytes());
            let _ = rx.decrypt(&TOKEN, &d);
        }));
        assert!(out.is_ok(), "UDP decrypt panicked on seq={seq}");
    }

    // A datagram below the anchor must not authenticate (it lands in epoch 0
    // and fails the AEAD); the window rejects seq 0 outright.
    let mut rx = DatagramCrypt::with_rekey(udp_state(4, DIR_LAUNCHER_TO_SERVER));
    let mut d = vec![0u8; TOKEN_LENGTH + 8 + 1 + 16];
    d[..TOKEN_LENGTH].copy_from_slice(&TOKEN);
    d[TOKEN_LENGTH..TOKEN_LENGTH + 8].copy_from_slice(&1u64.to_be_bytes());
    assert!(rx.decrypt(&TOKEN, &d).is_err());
}

// ---------------------------------------------------------------------------
// H6 — the k assert is unreachable from the wire
// ---------------------------------------------------------------------------
#[test]
fn p6_k_above_bound_trips_the_assert_but_not_from_the_wire() {
    assert_eq!(MAX_REKEY_LOG2, 63);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ = RekeyState::new(
            prk(),
            TRANSPORT_TCP,
            DIR_SERVER_TO_LAUNCHER,
            64,
            0,
            Arc::new(Aes256Gcm::new(
                SharedSecret::new(EPOCH0_KEY).as_ref().into(),
            )),
        );
    }));
    assert!(panicked.is_err(), "k=64 must trip the shift-safety assert");

    let st = RekeyState::new(
        prk(),
        TRANSPORT_TCP,
        DIR_SERVER_TO_LAUNCHER,
        63,
        0,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    );
    assert_eq!(st.epoch_of(u64::MAX), 1);
    assert_eq!(st.epoch_of((1u64 << 63) - 1), 0);
}

// ---------------------------------------------------------------------------
// H7 — cross-epoch replay is still rejected by the anti-replay machinery
// ---------------------------------------------------------------------------
#[test]
fn p7_cross_epoch_replay_is_rejected() {
    let k = 2u8;
    let state = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
    let mut tx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
    let mut rx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());

    let mut first = frame();
    tx.encrypt(1, 11, &mut first).unwrap();
    let mut c = first.clone();
    rx.decrypt(&mut c).unwrap();

    for _ in 0..10 {
        let mut b = frame();
        tx.encrypt(1, 11, &mut b).unwrap();
        rx.decrypt(&mut b).unwrap();
    }
    assert!(rx.current_seq() >= 2 << k, "receiver must be past epoch 1");

    let err = rx.decrypt(&mut first.clone()).unwrap_err().to_string();
    assert!(err.contains("replay attack detected"), "{err}");

    let mut fresh = frame();
    tx.encrypt(1, 11, &mut fresh).unwrap();
    let mut a = fresh.clone();
    rx.decrypt(&mut a).unwrap();
    assert!(
        rx.decrypt(&mut fresh.clone()).is_err(),
        "duplicate seq accepted"
    );
}

// ---------------------------------------------------------------------------
// H4(c) — forged fresh-epoch frames churn derivations without spending seqs
// ---------------------------------------------------------------------------
#[test]
fn p8_forged_fresh_epoch_frames_do_not_advance_the_counter() {
    let k = 8u8;
    let mut rx = Crypt::with_rekey(
        Arc::new(AtomicU64::new(0)),
        tcp_state(k, DIR_LAUNCHER_TO_SERVER),
    );
    for i in 1..=200u64 {
        let mut b = forged(i << k);
        assert!(
            rx.decrypt(&mut b).is_err(),
            "forged frame {i} authenticated"
        );
    }
    assert_eq!(
        rx.current_seq(),
        0,
        "failed frames must not advance the counter"
    );

    let mut tx = Crypt::with_rekey(
        Arc::new(AtomicU64::new(0)),
        tcp_state(k, DIR_LAUNCHER_TO_SERVER),
    );
    let mut legit = frame();
    tx.encrypt(1, 11, &mut legit).unwrap();
    assert_eq!(legit.seq().unwrap(), 1);
    rx.decrypt(&mut legit).unwrap();
    assert_eq!(rx.current_seq(), 2);
}

// ---------------------------------------------------------------------------
// H8 — derivation domain separation + KAT agreement
// ---------------------------------------------------------------------------
#[test]
fn p9_epoch_keys_are_domain_separated() {
    let material = derive_tunnel_material(&secret(), &ticket()).unwrap();
    assert_eq!(
        material.key_send.as_ref(),
        &[
            30, 79, 83, 235, 53, 71, 186, 71, 34, 250, 3, 51, 222, 193, 90, 208, 48, 112, 207, 208,
            219, 166, 191, 4, 208, 106, 159, 121, 221, 115, 30, 174
        ],
        "legacy epoch-0 material drifted"
    );

    let p = prk();
    // product KAT literal for (TCP, s2c, k=8, epoch=1)
    assert_eq!(
        p.expand_epoch_key(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 8, 1)
            .as_ref(),
        &[
            79, 111, 99, 128, 70, 211, 132, 77, 192, 222, 84, 130, 154, 162, 88, 239, 203, 189, 0,
            141, 50, 230, 46, 252, 14, 157, 248, 119, 251, 228, 250, 149
        ]
    );

    let mut seen: Vec<([u8; 32], String)> = Vec::new();
    let legacy = [
        *material.key_payload.as_ref(),
        *material.key_send.as_ref(),
        *material.key_receive.as_ref(),
    ];
    for &t in &[TRANSPORT_TCP, TRANSPORT_UDP] {
        for &d in &[DIR_SERVER_TO_LAUNCHER, DIR_LAUNCHER_TO_SERVER] {
            for k in [1u8, 8, 20, 63] {
                for e in 1..=8u64 {
                    let key = *p.expand_epoch_key(t, d, k, e).as_ref();
                    let label = format!("t{t} d{d} k{k} e{e}");
                    assert!(!legacy.contains(&key), "epoch key {label} == a legacy key");
                    for (prev, prev_label) in &seen {
                        assert_ne!(*prev, key, "epoch key collision: {label} == {prev_label}");
                    }
                    seen.push((key, label));
                }
            }
        }
    }
    assert_eq!(seen.len(), 2 * 2 * 4 * 8);

    // Epoch >= 1 is NEVER the legacy key (epoch 0 is served from the legacy
    // cipher, not from expand_epoch_key): a peer that implemented epoch 0 via
    // expand_epoch_key(.., 0) would diverge, which is exactly why the KAT
    // only pins epochs >= 1.
    let e0_via_expand = p.expand_epoch_key(TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, 8, 0);
    assert_ne!(e0_via_expand.as_ref(), material.key_send.as_ref());
    assert_ne!(e0_via_expand.as_ref(), material.key_receive.as_ref());
}

// ---------------------------------------------------------------------------
// H4(c) — derivation churn cost (first-hand numbers)
// ---------------------------------------------------------------------------
#[test]
fn p10_derivation_churn_cost() {
    let n = 50_000usize;
    let state = tcp_state(20, DIR_LAUNCHER_TO_SERVER);

    let mut honest_rx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
    let mut frames: Vec<PacketBuffer> = Vec::with_capacity(n);
    {
        let mut tx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
        for _ in 0..n {
            let mut b = frame();
            tx.encrypt(1, 11, &mut b).unwrap();
            frames.push(b);
        }
    }
    let t = Instant::now();
    for b in frames.iter_mut() {
        honest_rx.decrypt(b).unwrap();
    }
    let honest = t.elapsed();

    let mut churn_rx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
    let mut churn: Vec<PacketBuffer> = Vec::with_capacity(n);
    for i in 1..=n as u64 {
        let seq = i << 20; // one distinct epoch per frame
        let mut tx = Crypt::with_rekey(Arc::new(AtomicU64::new(seq - 1)), state.clone());
        let mut b = frame();
        tx.encrypt(1, 11, &mut b).unwrap();
        churn.push(b);
    }
    let t = Instant::now();
    for b in churn.iter_mut() {
        churn_rx.decrypt(b).unwrap();
    }
    let churned = t.elapsed();

    let per_honest = honest.as_nanos() as f64 / n as f64;
    let per_churn = churned.as_nanos() as f64 / n as f64;
    println!(
        "[p10] in-epoch {per_honest:.0} ns/frame vs fresh-epoch {per_churn:.0} ns/frame -> x{:.1}",
        per_churn / per_honest
    );
    assert!(
        per_churn / per_honest < 10.0,
        "churn amplification unexpectedly large: {per_churn:.0} vs {per_honest:.0} ns"
    );
}

// ---------------------------------------------------------------------------
// Cross-epoch UDP: the replay window is orthogonal to the epoch
// ---------------------------------------------------------------------------
#[test]
fn p11_udp_replay_window_is_orthogonal_to_epochs() {
    let k = 2u8;
    let mut tx = DatagramCrypt::with_rekey(udp_state(k, DIR_SERVER_TO_LAUNCHER));
    let mut rx = DatagramCrypt::with_rekey(udp_state(k, DIR_SERVER_TO_LAUNCHER));

    let mut wire = Vec::new();
    for i in 0..12 {
        wire.push(tx.encrypt(&TOKEN, format!("d{i}").as_bytes()).unwrap());
    }
    for &i in &[11usize, 6, 2, 0] {
        assert!(
            rx.decrypt(&TOKEN, &wire[i]).unwrap().is_some(),
            "datagram {i}"
        );
    }
    assert!(rx.decrypt(&TOKEN, &wire[11]).unwrap().is_none());
    assert!(rx.decrypt(&TOKEN, &wire[0]).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Shared counters across crypts + epochs (stream/recover overlap)
// ---------------------------------------------------------------------------
#[test]
fn p12_two_crypts_share_counter_across_epochs() {
    let k = 2u8;
    let state = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
    let counter = Arc::new(AtomicU64::new(0));
    let mut a = Crypt::with_rekey(counter.clone(), state.clone());
    let mut b = Crypt::with_rekey(counter.clone(), state.clone());

    let mut seqs = Vec::new();
    for i in 0..16u64 {
        let mut f = frame();
        if i % 2 == 0 {
            a.encrypt(1, 11, &mut f).unwrap();
        } else {
            b.encrypt(1, 11, &mut f).unwrap();
        }
        seqs.push(f.seq().unwrap());
    }
    for w in seqs.windows(2) {
        assert_eq!(w[1], w[0] + 1, "shared counter reused a seq");
    }
    assert_eq!(counter.load(Ordering::SeqCst), 16);
}

// ---------------------------------------------------------------------------
// The real factories agree across the epoch boundary (server <-> launcher view)
// ---------------------------------------------------------------------------
#[test]
fn p13_factories_agree_across_epochs() {
    let k = 3u8;
    let (mut server_in, mut server_out) = get_tunnel_crypts(
        &secret(),
        &ticket(),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        k,
    )
    .unwrap();
    let (mut launcher_send, mut launcher_recv) = get_tunnel_crypts(
        &secret(),
        &ticket(),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        k,
    )
    .unwrap();

    for i in 1..=60u64 {
        let mut a = frame();
        server_out.encrypt(1, 11, &mut a).unwrap();
        launcher_recv
            .decrypt(&mut a)
            .unwrap_or_else(|e| panic!("s2c frame {i} (epoch {}): {e}", i >> k));

        let mut b = frame();
        launcher_send.encrypt(1, 11, &mut b).unwrap();
        server_in
            .decrypt(&mut b)
            .unwrap_or_else(|e| panic!("c2s frame {i} (epoch {}): {e}", i >> k));
    }
}

// ---------------------------------------------------------------------------
// Differential model check: the ciphertext of every frame must equal
// AES-GCM(K_model(epoch(seq)), nonce=seq, aad=seq, channel||plaintext), where
// K_model(0) is the legacy key and K_model(e>=1) an INDEPENDENT HKDF-Expand
// (verified against RFC 5869 in Python). This catches any deviation in the
// epoch selection, cache or (transport, dir, k) folding.
// ---------------------------------------------------------------------------
use aes_gcm::aead::AeadInOut;

fn model_key(
    k: u8,
    transport: u8,
    dir: u8,
    epoch: u64,
    legacy_epoch0: &SharedSecret,
) -> SharedSecret {
    if epoch == 0 {
        legacy_epoch0.clone()
    } else {
        prk().expand_epoch_key(transport, dir, k, epoch)
    }
}

#[test]
fn p14_differential_epoch_key_selection_tcp() {
    let plaintext = b"rekey-probe";
    let legacy0 = SharedSecret::new(EPOCH0_KEY);
    for k in [1u8, 2, 3, 8, 20, 32] {
        let state = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
        let p = 1u64 << k;
        let mut seqs: Vec<u64> = vec![
            1,
            2,
            p - 1,
            p,
            p + 1,
            2 * p - 1,
            2 * p,
            2 * p + 1,
            3 * p - 1,
            3 * p,
            3 * p + 1,
        ];
        for j in 1..=8u64 {
            seqs.push((j.wrapping_mul(0x9E37_79B9_7F4A_7C15) % (4 * p)) + 1);
        }
        seqs.sort_unstable();
        seqs.dedup();

        for &seq in &seqs {
            let mut tx = Crypt::with_rekey(Arc::new(AtomicU64::new(seq - 1)), state.clone());
            let mut b = frame();
            tx.encrypt(1, plaintext.len(), &mut b).unwrap();
            assert_eq!(b.seq().unwrap(), seq, "k={k}");
            let got = b.buffer().unwrap()[10..].to_vec();

            let epoch = state.epoch_of(seq);
            let key = model_key(k, TRANSPORT_TCP, DIR_SERVER_TO_LAUNCHER, epoch, &legacy0);
            let cipher = Aes256Gcm::new(key.as_ref().into());
            let mut nonce_arr = [0u8; 12];
            nonce_arr[..8].copy_from_slice(&seq.to_be_bytes());
            let mut data = vec![0u8, 1]; // channel 1, big-endian
            data.extend_from_slice(plaintext);
            let mut buf = data;
            let tag = cipher
                .encrypt_inout_detached(
                    &aes_gcm::Nonce::from(nonce_arr),
                    &seq.to_be_bytes(),
                    (&mut buf[..]).into(),
                )
                .unwrap();
            let mut expected = buf;
            expected.extend_from_slice(tag.as_slice());
            assert_eq!(
                got, expected,
                "k={k} seq={seq} epoch={epoch}: wire bytes deviate from the model key"
            );
        }
    }
}

#[test]
fn p15_differential_epoch_key_selection_udp() {
    let k = 3u8;
    // The UDP epoch-0 key is the legacy UDP-leg key (HKDF label
    // "openuds-ticket-crypt-udp"), NOT the TCP key_send: pinned by the
    // product's `test_get_udp_crypts_known_answer`.
    let legacy0 = SharedSecret::new([
        115, 122, 103, 8, 221, 26, 166, 141, 102, 141, 74, 208, 99, 240, 91, 76, 233, 111, 200, 0,
        152, 79, 177, 241, 178, 56, 195, 87, 176, 182, 35, 9,
    ]);
    let (_, mut tx) = get_udp_crypts(&secret(), &ticket(), k).unwrap();
    let pt = b"udp-probe";
    for i in 1..=64u64 {
        let d = tx.encrypt(&TOKEN, pt).unwrap();
        let seq = u64::from_be_bytes(d[TOKEN_LENGTH..TOKEN_LENGTH + 8].try_into().unwrap());
        assert_eq!(seq, INITIAL_SEQ + i);
        let epoch = (seq - INITIAL_SEQ) >> k;
        let key = model_key(k, TRANSPORT_UDP, DIR_SERVER_TO_LAUNCHER, epoch, &legacy0);
        let cipher = Aes256Gcm::new(key.as_ref().into());
        let mut aad = [0u8; TOKEN_LENGTH + 8];
        aad[..TOKEN_LENGTH].copy_from_slice(&TOKEN);
        aad[TOKEN_LENGTH..].copy_from_slice(&seq.to_be_bytes());
        let mut nonce_arr = [0u8; 12];
        nonce_arr[..8].copy_from_slice(&seq.to_be_bytes());
        let mut buf = pt.to_vec();
        let tag = cipher
            .encrypt_inout_detached(
                &aes_gcm::Nonce::from(nonce_arr),
                &aad[..],
                (&mut buf[..]).into(),
            )
            .unwrap();
        let mut expected = buf;
        expected.extend_from_slice(tag.as_slice());
        assert_eq!(
            &d[TOKEN_LENGTH + 8..],
            expected.as_slice(),
            "udp datagram {i} epoch {epoch}: wire bytes deviate from the model key"
        );
    }
}

// ---------------------------------------------------------------------------
// set_rekey (the launcher's install-after-OpenResponse path): installing k
// mid-life must switch the key schedule with no wire change at the seam.
// ---------------------------------------------------------------------------
#[test]
fn p16_set_rekey_mid_life_switches_schedule() {
    let k = 2u8;
    let state = tcp_state(k, DIR_SERVER_TO_LAUNCHER);
    let counter = Arc::new(AtomicU64::new(0));
    // Launcher model: legacy crypt first (k unknown), then install k.
    let mut launcher = Crypt::with_counter(&SharedSecret::new(EPOCH0_KEY), counter.clone());
    launcher.set_rekey(state.clone());

    let mut server_rx = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state.clone());
    for i in 1..=30u64 {
        let mut b = frame();
        launcher.encrypt(1, 11, &mut b).unwrap();
        assert_eq!(b.seq().unwrap(), i);
        server_rx
            .decrypt(&mut b)
            .unwrap_or_else(|e| panic!("frame {i} (epoch {}): {e}", i >> k));
    }

    // Installing the SAME schedule on a crypt that already crossed epochs is
    // also consistent: the key is a pure function of the frame's seq.
    let mut late = Crypt::with_rekey(Arc::new(AtomicU64::new((5 << k) - 1)), state.clone());
    late.set_rekey(state.clone());
    let mut peer = Crypt::with_rekey(Arc::new(AtomicU64::new(0)), state);
    for _ in 0..4u64 {
        let mut b = frame();
        late.encrypt(1, 11, &mut b).unwrap();
        peer.decrypt(&mut b)
            .unwrap_or_else(|e| panic!("late-install frame: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Latent footgun (unreachable today): `cipher_for(epoch > 0)` on an
// `epoch0_only` state expands the placeholder ZERO PRK instead of rejecting,
// i.e. it would hand out a key anybody can compute. The only guard is the
// `k == 0` short-circuit inside `epoch_of`.
// ---------------------------------------------------------------------------
#[test]
fn p17_epoch0_only_epoch_gt0_trips_the_zero_prk_guard() {
    let off = RekeyState::epoch0_only(
        TRANSPORT_TCP,
        DIR_SERVER_TO_LAUNCHER,
        0,
        Arc::new(Aes256Gcm::new(
            SharedSecret::new(EPOCH0_KEY).as_ref().into(),
        )),
    );
    // Every wire-reachable seq maps to epoch 0, so `cipher_for(>0)` is never
    // reached through `cipher_for_seq`.
    assert_eq!(off.epoch_of(u64::MAX), 0);
    assert_eq!(off.epoch_of(1 << 40), 0);
    assert_eq!(off.epoch_of(1), 0);

    // The epoch > 0 path would expand `epoch0_only`'s all-zero PRK — a
    // publicly derivable key — so `cipher_for` now asserts it away. The
    // regression pins the guard: any future caller that reaches this path
    // must trip it, never silently derive from PRK = 0^32.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let reached = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| off.cipher_for(1)));
    std::panic::set_hook(previous_hook);
    assert!(
        reached.is_err(),
        "cipher_for(>0) with k == 0 must trip the zero-PRK guard"
    );
}
