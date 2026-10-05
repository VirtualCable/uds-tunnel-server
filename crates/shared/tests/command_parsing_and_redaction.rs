// Regression tests for the protocol-command parsing and the log-redaction
// surface: fuzz-style parsing, discriminant stability, and secret redaction.

use shared::log::{redact_secret, redact_secret_bytes};
use shared::protocol::Command;
use shared::protocol::ticket::Ticket;

/// Deterministic xorshift so the fuzz is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }
}

/// `from_slice` must never panic for any input; it must either parse or Err.
#[test]
fn command_from_slice_never_panics_exhaustive_short() {
    // Every 1- and 2-byte input.
    for a in 0u16..=255 {
        let _ = Command::from_slice(&[a as u8]);
        for b in 0u16..=255 {
            let _ = Command::from_slice(&[a as u8, b as u8]);
        }
    }
    // Every 3-byte input with the leading byte restricted to the defined
    // discriminants (0..=6) plus the boundaries (7, 254, 255).
    for a in [0u8, 1, 2, 3, 4, 5, 6, 7, 254, 255] {
        for b in 0u16..=255 {
            for c in [0u8, 1, 255] {
                let _ = Command::from_slice(&[a, b as u8, c]);
            }
        }
    }
}

/// Random fuzz up to the max error-message length + slack.
#[test]
fn command_from_slice_never_panics_random() {
    let mut rng = Rng(0x1234_5678_9abc_def0);
    for _ in 0..40_000 {
        let len = (rng.next() % 700) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        if !data.is_empty() {
            // Bias the leading byte towards real discriminants half the time.
            if rng.next().is_multiple_of(2) {
                data[0] = (rng.next() % 8) as u8;
            }
        }
        let _ = Command::from_slice(&data);
    }
}

/// Bytes 7..=254 and 255 must all be rejected (Unknown), 6 must be Nop,
/// 3 must be Close. This pins the wire discriminants: `Nop` must have been
/// appended, never inserted (which would have shifted Close/ChannelError).
#[test]
fn command_discriminants_are_stable() {
    for b in 7u16..=255 {
        assert!(
            Command::from_slice(&[b as u8]).is_err(),
            "byte {b} must be an unknown command"
        );
    }
    assert_eq!(Command::from_slice(&[6]).unwrap(), Command::Nop);
    assert_eq!(Command::from_slice(&[3]).unwrap(), Command::Close);
    assert_eq!(Command::from_slice(&[0]).unwrap(), Command::Ok);
    assert_eq!(Command::Nop.to_bytes(), vec![6u8]);
}

/// Trailing bytes after a `Nop` are ignored (accepted as Nop) — a client
/// cannot smuggle a second command inside one control frame, and the
/// inbound stream consumes the whole frame on Nop.
#[test]
fn nop_trailing_bytes_are_ignored() {
    for extra in [0u8, 1, 3, 6, 255] {
        assert_eq!(
            Command::from_slice(&[6, extra, extra]).unwrap(),
            Command::Nop
        );
    }
}

/// ChannelError / ConnectionError message slicing must never panic at any
/// length around the boundaries.
#[test]
fn error_message_slicing_boundaries() {
    for len in 0..700usize {
        let mut data = vec![4u8]; // ChannelError
        data.extend(std::iter::repeat_n(0x41u8, len));
        let _ = Command::from_slice(&data);

        let mut data = vec![5u8]; // ConnectionError
        data.extend(std::iter::repeat_n(0x41u8, len));
        let _ = Command::from_slice(&data);
    }
    // Multi-byte UTF-8 must not panic on encode (byte-sliced truncation).
    let msg = "é".repeat(400); // 800 bytes
    let bytes = Command::ChannelError {
        channel_id: 1,
        message: msg,
    }
    .to_bytes();
    let _ = Command::from_slice(&bytes);
}

/// Redaction must never panic and must not return the original value for
/// any length. Also record the residual entropy for the credential sizes
/// actually used by this protocol (48-char ticket, 32-byte secret).
#[test]
fn redaction_never_panics_and_hides_the_centre() {
    for len in 0..=300usize {
        let s: String = "A".repeat(len);
        let r = redact_secret(&s);
        assert!(!r.is_empty() || len == 0);
        if len >= 4 {
            assert!(r.contains("..."), "len {len}");
            assert_ne!(r, s);
        }
    }
    let ticket = "A".repeat(48);
    let rt = redact_secret(&ticket);
    assert_ne!(rt, ticket);
    assert!(rt.contains("..."));

    let secret = [0xABu8; 32];
    let rs = redact_secret_bytes(&secret);
    assert!(rs.contains("..."));
    // Document the leak: 64 hex chars -> 16 head + 16 tail hex chars visible
    // (= 8 + 8 of the 32 key bytes).
    let hex = "ab".repeat(32);
    let leaked_hex_chars = 64 - (64 / 2);
    let redacted_hex = redact_secret(&hex);
    assert_eq!(
        redacted_hex.replace("...", "").chars().count(),
        leaked_hex_chars
    );
    assert_eq!(redacted_hex.matches("ab").count(), leaked_hex_chars / 2);
}

/// A `Ticket`'s Debug/redacted form must never equal `as_str()` (the
/// functional value), and `as_str()`/`as_ref()` must stay byte-identical to
/// what the protocol derives keys from.
#[test]
fn ticket_redaction_does_not_touch_functional_value() {
    let raw = [b'Z'; 48];
    let t = Ticket::new(raw);
    assert_eq!(t.as_str(), std::str::from_utf8(&raw).unwrap());
    assert_eq!(t.as_ref(), &raw, "HKDF salt bytes must be raw");
    assert_ne!(t.redacted(), t.as_str());
    assert_ne!(format!("{t:?}"), t.as_str());
    assert!(format!("{t:?}").contains("..."));

    // notify-ticket parsing: pure length check, no alphanumeric validation.
    assert!(Ticket::try_from(&raw[..47]).is_err());
    assert!(Ticket::try_from(&raw[..]).is_ok());
    let mut long = raw.to_vec();
    long.push(b'X');
    assert!(Ticket::try_from(long.as_slice()).is_err());
}

// ---------------------------------------------------------------------------
// Unchecked arithmetic on an attacker-chosen sequence number.
//
// `Crypt::decrypt` used to run `self.seq.fetch_max(seq + 1)` where `seq` is
// read straight off the wire (the frame header). A peer that holds the session
// key can send a correctly-tagged frame with `seq == u64::MAX`, which overflowed
// `seq + 1`: panic under debug overflow-checks, counter wrap/wedge in release.
// The fix rejects `seq == u64::MAX` before the counter advance. This test pins
// the boundary so a regression re-introducing the unchecked add is caught.
// ---------------------------------------------------------------------------
#[test]
fn tcp_decrypt_seq_u64_max_boundary_is_rejected_not_faulted() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use shared::crypt::Crypt;
    use shared::crypt::types::{PacketBuffer, SharedSecret};

    let key = SharedSecret::new([0x5Au8; 32]);

    // Sender whose counter sits at u64::MAX - 1: the next encrypt uses
    // seq == u64::MAX (fetch_add returns the old value, +1 does NOT wrap).
    let mut sender = Crypt::with_counter(&key, Arc::new(AtomicU64::new(u64::MAX - 1)));
    let mut buf = PacketBuffer::new();
    buf.set_data(b"x").unwrap();
    sender.encrypt(1, 1, &mut buf).unwrap();
    assert_eq!(
        buf.seq().unwrap(),
        u64::MAX,
        "frame carries seq == u64::MAX"
    );

    // The receiver must reject it as an error in EVERY profile: no panic
    // (pre-fix debug behaviour), no accepted-frame-with-wrapped-counter
    // (pre-fix release behaviour).
    let mut receiver = Crypt::new(&key, 0);
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receiver.decrypt(&mut buf)));
    match outcome {
        Ok(res) => {
            assert!(
                res.is_err(),
                "seq == u64::MAX must be rejected: the frame must not be accepted"
            );
        }
        Err(_) => {
            panic!(
                "decrypt(seq == u64::MAX) panicked: the seq+1 overflow guard was lost \
                 (the fix rejects the frame before advancing the counter)"
            );
        }
    }
    assert_eq!(
        receiver.current_seq(),
        0,
        "a rejected seq == u64::MAX must not move the counter"
    );
}
