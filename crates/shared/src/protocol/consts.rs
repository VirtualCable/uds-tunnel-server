// Handshake constants
pub const HANDSHAKE_V2_SIGNATURE: &[u8; 8] = b"\x5AMGB\xA5\x02\x00\x00";
pub const HANDSHAKE_TIMEOUT_MS: u64 = 200;
pub const MAX_HANDSHAKE_RETRIES: u8 = 3; // After this, we can block the IP for a while
