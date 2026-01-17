pub const MAX_PACKET_SIZE: usize = 4096; // Hard limit for packet size. Anythig abobe this will be rejected.
pub const HEADER_LENGTH: usize = 8 + 2; // counter (8 bytes) + length (2 bytes)
pub const TAG_LENGTH: usize = 16; // AES-GCM tag length
// IPv6 minimum MTU is 1280 bytes, minus IP (40 bytes) and UDP (8 bytes, future) headers - leaves 1232 bytes for payload
// We use 1200 + HEADER_LENGTH + TAG_LENGTH = 1226 bytes to have some margin
pub const CRYPT_PACKET_SIZE: usize = 1200; // This is our preferred packet size for encryption/decryption