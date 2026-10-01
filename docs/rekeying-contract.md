# Rekeying tunnel — cross-repo contract (server ↔ launcher)

This is the **source of truth** for the per-epoch key rotation shared by
`uds-tunnel-server` and the UDS launcher (`uds-client`). Both sides must
implement it identically: any drift is caught by the mirrored KATs and the
AEAD check (hard, immediate failure — never silent corruption). Design
rationale and NIST justification live in `docs/plan/rekeying.md` (internal,
not shipped); this file only pins the wire + crypto contract.

Code references (server): `shared::crypt::rekey` (`SessionPrk`, `RekeyState`,
`epoch_of`, `expand_epoch_key`), `OpenResponse` in
`server::connection::types`, session pinning in `server::session`.
Launcher mirror: `crypt::rekey` and `connection::v5::proxy`.

## 1. Wire format

- `OpenResponse` is **91 bytes** (`rekey_log2: u8` at offset **84**, right
  after `udp_port`). The pre-rekeying 90-byte layout is **rejected**, as is
  any other length — atomic rollout, no compatibility gating.
- `rekey_log2` encodes `k` (log2 of the rekey threshold): `0` = **OFF**
  (single key for the whole session, byte-identical to the pre-feature
  behaviour); `1..=63` = rotate every `2^k` sequences. **`k >= 64` is a hard
  handshake rejection** (`seq >> k` would be an undefined shift).
- The default is `k = 20` (`DEFAULT_REKEY_LOG2`); the server always advertises
  its configured value.
- `OpenResponse` travels AEAD-sealed under epoch 0, so `rekey_log2` is
  authenticated by ticket possession and cannot be tampered with in transit.

## 2. Epoch function (`epoch_of`)

```
epoch(seq) = saturating_sub(seq, seq_base) >> k        // k >= 1
epoch(seq) = 0                                         // k == 0 (OFF)
```

- `seq_base` is **anchored per transport**: `0` for TCP (counters start near
  zero) and `2^63` (`datagram::INITIAL_SEQ`) for UDP — the first datagram of
  a session must sit in epoch 0 no matter how large its absolute seq is.
  **UDP is not `seq >> k` on the raw seq**: that would put a fresh session at
  epoch `2^(63-k)` and break lockstep with a peer that anchors at `seq_base`.
- `saturating_sub` means wire-crafted seqs below the anchor land in epoch 0
  instead of underflowing; they then fail AEAD like any other forgery.
- Consequence (accepted): an emitter that starts its UDP counter below
  `seq_base` saturates to epoch 0 and keeps one key for up to `2^63`
  datagrams — the per-key NIST bound does not bind in that direction. No
  attack benefit (the emitter holds the key); documented deliberately.

## 3. Epoch key derivation

- PRK per session: `HKDF-Extract(salt = ticket, IKM = shared_secret)`
  (`SessionPrk`), computed **once**, owned by the `Session`, shared by all
  crypts via `Arc`.
- **Epoch 0**: the legacy tunnel material (`derive_tunnel_material`,
  `key_send`/`key_receive`; UDP `get_udp_crypts`). Existing epoch-0 KATs stay
  valid; OFF is byte-exact the pre-feature wire.
- **Epoch n >= 1**:

  ```
  info = "openuds-tunnel-rekey"          // 20 B, fixed label
       || transport_be8                  // 0 = TCP, 1 = UDP
       || dir_be8                        // 0 = server→launcher, 1 = launcher→server
       || k_be8                          // domain separation: different thresholds never share epoch keys
       || epoch_be64                     // n, big-endian
  K(dir, n) = HKDF-Expand(PRK, info, 32) // SHA-256
  ```

- `key_payload` / `nonce_payload` of the ticket do NOT rotate (they live what
  the handshake lives). The per-direction `seq` counters are untouched: the
  epoch only changes the key; nonce/AAD = `seq` stays injective across the
  whole session, so a frame in flight from the previous epoch decrypts
  correctly with zero transition state. **No dual-key window, no REKEY_REQ/ACK
  control messages.**

## 4. Session pinning and `Recover`

- `k` is a **session property** captured at handshake, persisted in `Session`
  alongside `seq_in`/`seq_out`, and **never renegotiated**: a `Recover`
  re-advertises the session's `k` (it must not re-read the server config), and
  the server rebuilds crypts from `session.k`.
- The launcher **adopts `k` from its first `OpenResponse` and pins it**; if a
  later `Recover` advertises a different `k`, it **aborts hard** (no reconnect,
  no re-pinning). A drifted peer is rejected by AEAD anyway; the abort just
  makes the failure legible.
- The launcher installs `k` into its crypts **before** handling the ticket on
  `Recover` (the ticket confirm itself can land in epoch > 0).
- UDP on `Recover`: the server preserves the replay window and `send_seq` —
  the launcher must **not re-seed** the UDP crypts, and must **not** build
  them with `DatagramCrypt::new` (k=0) against a server with `k > 0`.
  `DatagramCrypt` has no `set_rekey` (only `Crypt` does): installing `k` on
  UDP rebuilds the crypts, which loses the replay window — known cost, keep it
  in mind when touching that path.

## 5. Replay-seq guard

- The launcher rejects `seq == u64::MAX` at decrypt time (mirror of server-side
  vuln-0016): the counter must never saturate past the wire-valid range.

## 6. Keepalive interaction

- `Nop` on channel 0 must be sent by the launcher every **< 10 s** (design
  target 2 s). It sustains the *leg*, never the *session*: Nops do not count
  as payload data for `session_idle_data_timeout_secs`. This is a hard
  requirement of the monolithic release (idle recycling would otherwise kill
  quiet-but-alive sessions).

## 7. Mirrored known-answer tests

Both repos carry **identical literal KATs** (decimal literals computed
independently of either implementation, so drift on one side breaks the test
on the other):

- `rekey_epoch_keys_known_answer` — `expand_epoch_key` for TCP/UDP × both dirs
  × epochs 1–2 with `k = 8`, fixed secret/ticket.
- `epoch_of_known_answer` — 14 cases incl. the `k = 0` rule, both anchors,
  below-anchor saturation, `seq = u64::MAX` boundaries, and `k = 1..=63`.

Server: `rekey_epoch_keys_known_answer` in `crates/shared/src/crypt/tunnel.rs`,
`epoch_of_known_answer` in `crates/shared/src/crypt/rekey.rs` (+ the
differential/adversarial suite `crates/shared/tests/crypt_rekey_regressions.rs`
and the wire-abuse suite `crypt_rekey_wire_abuse.rs`). Launcher:
`crates/crypt/src/rekey.rs` (both KATs).

## 8. Security posture (what this does NOT give)

Per-key exposure bounding (NIST SP 800-38D q/σ bounds) only. **No retrospective
forward secrecy, no PCS**: compromising the ticket secret derives every epoch.
If that ever changes, it is a protocol revision, not this contract.

## 9. Why `k = 20` by default (NIST SP 800-38D / McGrew-Villa)

This repo's construction: nonce = `seq_be64 || zeros(4)`, 1 tag per frame,
frames <= `MAX_PACKET_SIZE = 4096` B (preferred 1200 B). The epoch bounds `q`
(invocations/key) and `sigma` (blocks/key) simultaneously:

| Bound | NIST value | With k=20 here |
| --- | --- | --- |
| AEAD invocations/key `q` | `2^32` | `2^20` -> margin x4096 |
| Blocks/key `sigma` (32-bit internal counter) | `2^32` blocks (~68 GB) | `2^20 * 256 = 2^28` worst case; `2^26.2` at preferred frame size |
| Forge advantage ~ `2*q*sigma/2^96 < 2^-32` => `q*sigma <= 2^64` | `2^64` | `2^28` -> margin `2^36` |

`2^20` is ultra-conservative; it satisfies every bound with wide margin.
