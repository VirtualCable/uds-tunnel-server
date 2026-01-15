pub const VERSION: &str = "v5.0.0";

#[cfg(debug_assertions)]
pub const DEFAULT_LOG_LEVEL: &str = "debug";
#[cfg(debug_assertions)]
pub const CONFIGFILE_PATH: &str = "udstunnel.conf";

#[cfg(not(debug_assertions))]
pub const DEFAULT_LOG_LEVEL: &str = "info";
#[cfg(not(debug_assertions))]
pub const CONFIGFILE_PATH: &str = "/etc/udstunnel.conf";

// Handshake constants
pub const HANDSHAKE_V2_SIGNATURE: &[u8; 8] = b"\x5AMGB\xA5\x02\x00\x00";
pub const HANDSHAKE_TIMEOUT_MS: u64 = 200;
pub const MAX_HANDSHAKE_RETRIES: u8 = 3;  // After this, we can block the IP for a while

// Ticket related constants
pub const TICKET_LENGTH: usize = 48;

// HTTP related constants
pub const USER_AGENT: &str = "UDSTunnel/5.0.0";

// Channel related constants
pub const CHANNEL_SIZE: usize = 2048; // 2k messages as much on a channel buffer
