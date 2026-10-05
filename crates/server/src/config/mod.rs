use crate::consts::{
    CONFIGFILE_PATH, DEFAULT_MAX_SESSIONS, DEFAULT_SESSION_IDLE_DATA_TIMEOUT_SECS,
};
use shared::log;
use std::{
    fs::read_to_string,
    net::SocketAddr,
    sync::{Arc, OnceLock, RwLock},
};

#[derive(serde::Deserialize)]
pub struct ServerConfig {
    pub listen_addr: Option<String>, // * = all interfaces, else IP address, default: *
    pub log_level: Option<String>,   // Log level for the server, default: "info"
    pub listen_port: Option<u16>,    // Port to listen on, default: 443
    pub use_proxy_protocol: Option<bool>, // Whether to expect PROXY protocol v2 headers, default: false
    pub udp_listen_port: Option<u16>, // Port for the shared UDP relay socket, default: same as listen_port
    pub udp_enabled: Option<bool>, // Master switch for the UDP relay leg, default: true (the real gate is the broker flag)
    pub ticket_api_url: String, // URL of the broker API, e.g., https://broker.example.com/uds/rest/ticket
    // SECURITY: setting this to `true` disables TLS certificate validation
    // on the broker API client. Useful for diagnostics against
    // self-signed brokers; **never** enable in production. Default `false`.
    pub dangerous_disable_ssl_verify: Option<bool>,
    pub broker_auth_token: String, // Auth token for the broker API
    pub recovery_buffer_size: Option<usize>, // Size of the session recovery buffer in Kb, default: 64 (kb)
    pub max_sessions: Option<usize>, // Hard cap on concurrent sessions, default: DEFAULT_MAX_SESSIONS (8192)
    pub max_sessions_per_remote: Option<usize>, // Per-source-IP cap. None = disabled (no per-IP check).
    pub session_idle_data_timeout_secs: Option<u64>, // Data-idle session cap. None = DEFAULT (120), Some(0) = disabled.
    pub rekey_seq_log2: Option<u8>, // Rekey threshold: log2 frames per AES-GCM key epoch. None = DEFAULT (20), Some(0) = OFF, Some(1..=63) = that threshold.
}

impl ServerConfig {
    pub fn from_toml_str(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Effective session cap, falling back to the default when unset.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions.unwrap_or(DEFAULT_MAX_SESSIONS)
    }

    /// Effective data-idle cap for sessions. `None` means the cap is
    /// disabled (explicit `0` in the config); otherwise the deadline a
    /// session may go without payload bytes (data channels only — keep-
    /// alive `Nop` frames never count) before it is ended. Unset falls
    /// back to `DEFAULT_SESSION_IDLE_DATA_TIMEOUT_SECS`.
    pub fn session_idle_data_timeout(&self) -> Option<std::time::Duration> {
        match self.session_idle_data_timeout_secs {
            Some(0) => None,
            Some(secs) => Some(std::time::Duration::from_secs(secs)),
            None => Some(std::time::Duration::from_secs(
                DEFAULT_SESSION_IDLE_DATA_TIMEOUT_SECS,
            )),
        }
    }

    /// Effective rekeying threshold (`k`, log2 of the frames per key epoch)
    /// advertised to launchers in `OpenResponse.rekey_log2` and pinned to
    /// every session. `None` falls back to `DEFAULT_REKEY_LOG2` (2^20
    /// frames per key); `Some(0)` disables rekeying entirely (single key
    /// for the session lifetime, the pre-rekeying wire format). Values
    /// above the shift-safe bound (`63`) would make `seq >> k` undefined,
    /// so they are clamped with a warning instead of poisoning the
    /// handshake (the launcher-side bound is a hard rejection).
    pub fn rekey_log2(&self) -> u8 {
        match self.rekey_seq_log2 {
            Some(k) if k > shared::crypt::rekey::MAX_REKEY_LOG2 => {
                log::warn!(
                    "rekey_seq_log2 = {k} exceeds the shift-safe bound {}; clamping to it",
                    shared::crypt::rekey::MAX_REKEY_LOG2
                );
                shared::crypt::rekey::MAX_REKEY_LOG2
            }
            Some(k) => k,
            None => shared::crypt::rekey::DEFAULT_REKEY_LOG2,
        }
    }

    /// Whether the UDP relay leg is allowed at all on this server.
    /// Default `true`; the per-session gate is the broker's
    /// `enable_udp` flag (checked in `connection::connect`).
    pub fn udp_enabled(&self) -> bool {
        self.udp_enabled.unwrap_or(true)
    }

    /// UDP relay bind address: same interface as TCP, UDP port falls
    /// back to `listen_port` when `udp_listen_port` is unset.
    pub fn udp_sockaddr(&self) -> SocketAddr {
        let mut addr = self.listen_sockaddr();
        addr.set_port(self.udp_listen_port.unwrap_or(addr.port()));
        addr
    }

    /// Logs `warn!` entries for any configuration knobs that materially weaken
    /// the security posture of the tunnel server. Called from `main` after the
    /// logger has been initialized so the warnings actually show up.
    ///
    /// Each check is intentionally explicit and self-contained — adding a new
    /// dangerous setting is just one more `if` block here.
    pub fn report_dangerous_settings(&self) {
        // TLS cert verification disabled on broker API client.
        if self.dangerous_disable_ssl_verify.unwrap_or(false) {
            log::warn!(
                "dangerous_disable_ssl_verify = true: TLS certificate validation is DISABLED for broker API requests. \
                 This exposes the connection to man-in-the-middle attacks. \
                 Use only for diagnostics against self-signed brokers; never in production."
            );
        }
        // Rekeying explicitly switched off. The per-epoch key rotation exists
        // to bound AEAD invocations per key (NIST SP 800-38D); `0` gives up
        // that bound for the whole session lifetime.
        if self.rekey_seq_log2 == Some(0) {
            log::warn!(
                "rekey_seq_log2 = 0: tunnel key rotation is DISABLED. Every frame of a \
                 session is encrypted under one key, with no bound on AEAD invocations \
                 per key (NIST SP 800-38D). Use only for pre-rekeying compatibility \
                 diagnostics; never in production."
            );
        }
        // Data-idle session recycling explicitly disabled: a session that
        // stops carrying payload lives forever (leg keep-alive Nops do not
        // count as data), widening the window of a leaked-but-open tunnel.
        if self.session_idle_data_timeout_secs == Some(0) {
            log::warn!(
                "session_idle_data_timeout_secs = 0: idle-session recycling is DISABLED. \
                 Sessions without payload traffic are never ended. Prefer the default \
                 ({} s) unless a broker-side lifecycle fully replaces it.",
                DEFAULT_SESSION_IDLE_DATA_TIMEOUT_SECS
            );
        }
    }

    pub fn listen_sockaddr(&self) -> SocketAddr {
        let addr_str = self
            .listen_addr
            .as_deref()
            .unwrap_or("*")
            .replace("*", "0.0.0.0")
            .to_string();

        let port = self.listen_port.unwrap_or(443);
        SocketAddr::new(addr_str.parse().unwrap(), port)
    }
}

pub fn get() -> Arc<RwLock<ServerConfig>> {
    // Global shared configuration, maybe modified on runtime (and by tests also)
    // so it's convenient to have it behind a RwLock
    static SERVER_CONFIG: OnceLock<Arc<RwLock<ServerConfig>>> = OnceLock::new();

    // Note: Default config is not usable, but allow to start the server without a config file
    SERVER_CONFIG
        .get_or_init(|| {
            let mut config = if let Ok(config_str) = read_to_string(CONFIGFILE_PATH) {
                ServerConfig::from_toml_str(&config_str)
                    .expect("Failed to parse server configuration file")
            } else {
                ServerConfig {
                    log_level: None,
                    listen_addr: None,
                    listen_port: None,
                    use_proxy_protocol: None,
                    udp_listen_port: None,
                    udp_enabled: None,
                    ticket_api_url: "".to_string(),
                    dangerous_disable_ssl_verify: None,
                    broker_auth_token: "".to_string(),
                    recovery_buffer_size: None,
                    max_sessions: None,
                    max_sessions_per_remote: None,
                    session_idle_data_timeout_secs: None,
                    rekey_seq_log2: None,
                }
            };

            // Override with environment variables if present
            if let Ok(addr) = std::env::var("UDSTUNNEL_LISTEN_ADDR") {
                config.listen_addr = Some(addr);
            }
            if let Ok(port_str) = std::env::var("UDSTUNNEL_LISTEN_PORT")
                && let Ok(port) = port_str.parse::<u16>()
            {
                config.listen_port = Some(port);
            }
            if let Ok(port_str) = std::env::var("UDSTUNNEL_UDP_LISTEN_PORT")
                && let Ok(port) = port_str.parse::<u16>()
            {
                config.udp_listen_port = Some(port);
            }

            Arc::new(RwLock::new(config))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let toml_str = r#"
            listen_addr = "127.0.0.1"
            listen_port = 443
            use_proxy_protocol = true
            ticket_api_url = "https://broker.example.com/uds/rest/ticket"
            dangerous_disable_ssl_verify = true
            broker_auth_token = "test_token"
        "#;
        let config = ServerConfig::from_toml_str(toml_str).unwrap();
        assert_eq!(config.listen_addr, Some("127.0.0.1".to_string()));
        assert_eq!(config.listen_port, Some(443));
        assert_eq!(config.use_proxy_protocol, Some(true));
        assert_eq!(
            config.ticket_api_url,
            "https://broker.example.com/uds/rest/ticket".to_string()
        );
        assert_eq!(config.dangerous_disable_ssl_verify, Some(true));
        assert_eq!(config.broker_auth_token, "test_token".to_string());
        // UDP knobs unset: relay enabled by default, port follows listen_port
        assert_eq!(config.udp_listen_port, None);
        assert!(config.udp_enabled());
        assert_eq!(config.udp_sockaddr(), "127.0.0.1:443".parse().unwrap());
    }

    /// `rekey_seq_log2`: unset resolves to `DEFAULT_REKEY_LOG2`; explicit
    /// values (0 = OFF, 8, 63) resolve as-is; out-of-bound `>= 64` clamps
    /// to the shift-safe bound rather than poisoning the handshake with an
    /// undefined `seq >> k`.
    #[test]
    fn test_parse_config_rekey_log2() {
        let base = r#"
            ticket_api_url = "https://broker.example.com/uds/rest/ticket"
            broker_auth_token = "test_token"
        "#;

        // unset -> default (20)
        let config = ServerConfig::from_toml_str(base).unwrap();
        assert_eq!(config.rekey_seq_log2, None);
        assert_eq!(
            config.rekey_log2(),
            shared::crypt::rekey::DEFAULT_REKEY_LOG2
        );

        // explicit 0 -> OFF
        let config =
            ServerConfig::from_toml_str(&(base.to_string() + "\nrekey_seq_log2 = 0")).unwrap();
        assert_eq!(config.rekey_log2(), 0);

        // explicit 8
        let config =
            ServerConfig::from_toml_str(&(base.to_string() + "\nrekey_seq_log2 = 8")).unwrap();
        assert_eq!(config.rekey_log2(), 8);

        // explicit 63 (max shift-safe)
        let config =
            ServerConfig::from_toml_str(&(base.to_string() + "\nrekey_seq_log2 = 63")).unwrap();
        assert_eq!(config.rekey_log2(), shared::crypt::rekey::MAX_REKEY_LOG2);

        // out-of-range 64 / 128 / 200 -> clamped to MAX_REKEY_LOG2
        for raw in [64u8, 128, 200, 255] {
            let toml = base.to_string() + &format!("\nrekey_seq_log2 = {raw}");
            let config = ServerConfig::from_toml_str(&toml).unwrap();
            assert_eq!(
                config.rekey_log2(),
                shared::crypt::rekey::MAX_REKEY_LOG2,
                "k = {raw} must clamp, not propagate an undefined shift"
            );
        }
    }

    /// `rekey_seq_log2` must parse as a `u8` (1..=255), never silently
    /// accept a non-integer or a value wider than the wire's single byte.
    /// A TOML type error propagates (the config file must not start the
    /// server with a half-read knob).
    #[test]
    fn test_parse_config_rekey_log2_type_errors() {
        let base = r#"
            ticket_api_url = "https://broker.example.com/uds/rest/ticket"
            broker_auth_token = "test_token"
        "#;
        // Non-integer (string) rejected by serde
        let bad_str = base.to_string() + "\nrekey_seq_log2 = \"8\"";
        assert!(ServerConfig::from_toml_str(&bad_str).is_err());
        // Above u8 range rejected by serde
        let bad_num = base.to_string() + "\nrekey_seq_log2 = 300";
        assert!(ServerConfig::from_toml_str(&bad_num).is_err());
    }

    #[test]
    fn test_parse_config_udp_overrides() {
        let toml_str = r#"
            listen_addr = "*"
            listen_port = 443
            udp_listen_port = 8443
            udp_enabled = false
            ticket_api_url = "https://broker.example.com/uds/rest/ticket"
            broker_auth_token = "test_token"
        "#;
        let config = ServerConfig::from_toml_str(toml_str).unwrap();
        assert_eq!(config.udp_listen_port, Some(8443));
        assert!(!config.udp_enabled());
        assert_eq!(config.udp_sockaddr(), "0.0.0.0:8443".parse().unwrap());
    }
}
