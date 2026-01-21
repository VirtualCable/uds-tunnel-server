use std::{
    fs::read_to_string,
    sync::{Arc, OnceLock, RwLock},
};

use crate::consts::CONFIGFILE_PATH;

#[derive(serde::Deserialize)]
pub struct ServerConfig {
    pub listen_addr: Option<String>, // * = all interfaces, else IP address, default: *
    pub listen_port: Option<u16>,    // Port to listen on, default: 443
    pub use_proxy_protocol: Option<bool>, // Whether to expect PROXY protocol v2 headers, default: false
    pub ticket_api_url: String, // URL of the broker API, e.g., https://broker.example.com/uds/rest/ticket
    pub verify_ssl: Option<bool>, // Whether to verify SSL certificates on broker API: default: true
    pub broker_auth_token: String, // Auth token for the broker API
}

impl ServerConfig {
    pub fn from_toml_str(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }
}

pub fn get() -> Arc<RwLock<ServerConfig>> {
    // Global shared configuration, maybe modified on runtime (and by tests also)
    // so it's convenient to have it behind a RwLock
    static SERVER_CONFIG: OnceLock<Arc<RwLock<ServerConfig>>> = OnceLock::new();

    // Note: Default config is not usable, but allow to start the server without a config file
    SERVER_CONFIG
        .get_or_init(|| {
            if let Ok(config_str) = read_to_string(CONFIGFILE_PATH) {
                let config = ServerConfig::from_toml_str(&config_str)
                    .expect("Failed to parse server configuration file");
                Arc::new(RwLock::new(config))
            } else {
                Arc::new(RwLock::new(ServerConfig {
                    listen_addr: None,
                    listen_port: None,
                    use_proxy_protocol: None,
                    ticket_api_url: "".to_string(),
                    verify_ssl: None,
                    broker_auth_token: "".to_string(),
                }))
            }
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
            verify_ssl = false
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
        assert_eq!(config.verify_ssl, Some(false));
        assert_eq!(config.broker_auth_token, "test_token".to_string());
    }
}
