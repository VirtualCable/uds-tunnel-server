// Regression tests for the wire sequence-number boundary in `Crypt::decrypt`.
use shared::crypt::{
    Crypt,
    types::{PacketBuffer, SharedSecret},
};

/// A peer that holds the key can put any `seq` on the wire (it encrypts with
/// that seq, so the tag verifies). Pre-fix, the inbound `Crypt` computed
/// `seq + 1` straight from the wire header, which panicked on a debug build
/// (overflow-checks) or wedged/wrapped the counter on a release build for
/// `seq = u64::MAX` — a one-frame DoS on an unauthenticated stream. The fix
/// rejects `seq == u64::MAX` before the counter advance, so the frame is a
/// plain error and the counter is untouched, in every build profile.
#[test]
fn crafted_max_seq_on_the_wire_is_rejected_without_faulting_the_stream() {
    let key = SharedSecret::new([7u8; 32]);
    let mut attacker = Crypt::new(&key, u64::MAX - 1);
    let mut buf = PacketBuffer::from(b"boom".as_slice());
    attacker.encrypt(1, 4, &mut buf).unwrap();
    assert_eq!(
        buf.seq().unwrap(),
        u64::MAX,
        "attacker frame carries seq=MAX"
    );

    let mut server = Crypt::new(&key, 0);
    let mut copy = buf.clone();
    // Must not panic in any profile (pre-fix: debug panicked outright).
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.decrypt(&mut copy)));
    assert!(
        outcome.is_ok(),
        "decrypt(seq=MAX) must not panic: the seq+1 overflow guard was lost"
    );
    let res = outcome.unwrap();
    assert!(
        res.is_err(),
        "the u64::MAX frame must be rejected, not accepted"
    );
    assert!(
        res.err().unwrap().to_string().contains("u64::MAX"),
        "expected the explicit seq rejection, got the error above"
    );
    // The rejection is before the counter advance: the stream is fully usable.
    assert_eq!(
        server.current_seq(),
        0,
        "a rejected seq=MAX frame must not move the inbound counter"
    );

    let mut legit = Crypt::new(&key, 0);
    let mut lb = PacketBuffer::from(b"hi".as_slice());
    legit.encrypt(1, 2, &mut lb).unwrap();
    let mut lcopy = lb.clone();
    server
        .decrypt(&mut lcopy)
        .expect("a legitimate frame after a rejected seq=MAX must still decrypt");
    assert_eq!(server.current_seq(), 2);
}

/// The boundary one below the maximum is still accepted: the fix must reject
/// only `u64::MAX`, not merely large sequence numbers.
#[test]
fn crafted_max_minus_one_seq_on_the_wire_is_still_accepted() {
    let key = SharedSecret::new([7u8; 32]);
    let mut attacker = Crypt::new(&key, u64::MAX - 2);
    let mut buf = PacketBuffer::from(b"boom".as_slice());
    attacker.encrypt(1, 4, &mut buf).unwrap();
    assert_eq!(buf.seq().unwrap(), u64::MAX - 1);

    let mut server = Crypt::new(&key, 0);
    let mut copy = buf.clone();
    let r = server.decrypt(&mut copy);
    assert!(
        r.is_ok(),
        "seq=MAX-1 is below the rejection boundary and must decrypt, got: {:?}",
        r.err()
    );
    assert_eq!(
        server.current_seq(),
        u64::MAX,
        "counter must advance past MAX-1"
    );

    // A later, entirely legitimate frame (small seq) is now rejected.
    let mut legit = Crypt::new(&key, 0);
    let mut lb = PacketBuffer::from(b"hi".as_slice());
    legit.encrypt(1, 2, &mut lb).unwrap();
    let mut lcopy = lb.clone();
    let lr = server.decrypt(&mut lcopy);
    assert!(
        lr.is_err(),
        "a small-seq frame after a near-MAX one must be rejected as replay"
    );
}
