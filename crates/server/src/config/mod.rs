use std::{
    fs::read_to_string,
    net::SocketAddr,
    sync::{Arc, OnceLock, RwLock},
};

use crate::consts::{CONFIGFILE_PATH, DEFAULT_MAX_SESSIONS};

#[derive(serde::Deserialize)]
pub struct ServerConfig {
    pub listen_addr: Option<String>, // * = all interfaces, else IP address, default: *
    pub log_level: Option<String>,   // Log level for the server, default: "info"
    pub listen_port: Option<u16>,    // Port to listen on, default: 443
    pub use_proxy_protocol: Option<bool>, // Whether to expect PROXY protocol v2 headers, default: false
    pub ticket_api_url: String, // URL of the broker API, e.g., https://broker.example.com/uds/rest/ticket
    // SECURITY: setting this to `true` disables TLS certificate validation
    // on the broker API client. Useful for diagnostics against
    // self-signed brokers; **never** enable in production. Default `false`.
    pub dangerous_disable_ssl_verify: Option<bool>,
    pub broker_auth_token: String, // Auth token for the broker API
    pub recovery_buffer_size: Option<usize>, // Size of the session recovery buffer in Kb, default: 64 (kb)
    pub max_sessions: Option<usize>, // Hard cap on concurrent sessions, default: DEFAULT_MAX_SESSIONS (8192)
    pub max_sessions_per_remote: Option<usize>, // Per-source-IP cap. None = disabled (no per-IP check).
}

impl ServerConfig {
    pub fn from_toml_str(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Effective session cap, falling back to the default when unset.
    pub fn max_sessions(&self) -> usize {
        self.max_sessions.unwrap_or(DEFAULT_MAX_SESSIONS)
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
                    ticket_api_url: "".to_string(),
                    dangerous_disable_ssl_verify: None,
                    broker_auth_token: "".to_string(),
                    recovery_buffer_size: None,
                    max_sessions: None,
                    max_sessions_per_remote: None,
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
    }
}
