// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot compub mod broker;
use std::net::SocketAddr;

use anyhow::Result;
use hickory_resolver::{Resolver, config::*, name_server::TokioConnectionProvider};
use reqwest::Client;

use crate::config;
use shared::{crypt::types::SharedSecret, log, ticket::Ticket};

#[derive(serde::Deserialize, Debug)]
pub struct TicketRemote {
    pub host: String,
    pub port: u16,
    pub stream_channel_id: u16,
}

#[derive(serde::Deserialize, Debug)]
pub struct TicketResponse {
    pub remotes: Vec<TicketRemote>,
    pub notify: String, // Stop notification ticket
    pub shared_secret: Option<String>,
}

impl TicketResponse {
    pub fn get_shared_secret(&self) -> Result<SharedSecret> {
        if let Some(ref secret_str) = self.shared_secret {
            SharedSecret::from_hex(secret_str)
        } else {
            Err(anyhow::anyhow!("Missing or invalid shared secret"))
        }
    }

    pub async fn target_addr(&self, stream_channel_id: u16) -> Result<SocketAddr> {
        let remote = self
            .remotes
            .iter()
            .find(|r| r.stream_channel_id == stream_channel_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No remote found for stream channel id: {}",
                    stream_channel_id
                )
            })?;

        let resolver = Resolver::builder_with_config(
            ResolverConfig::default(),
            TokioConnectionProvider::default(),
        )
        .build();

        match resolver.lookup_ip(&remote.host).await {
            Ok(lookup) => {
                let ip = lookup.iter().next().ok_or_else(|| {
                    anyhow::anyhow!("No IP addresses found for host: {}", remote.host)
                })?;
                Ok(SocketAddr::new(ip, remote.port))
            }
            Err(e) => Err(anyhow::anyhow!(
                "DNS resolution failed for {}: {}",
                remote.host,
                e
            )),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.remotes.is_empty() {
            return Err(anyhow::anyhow!("No remotes in ticket response"));
        }
        for remote in &self.remotes {
            if remote.host.is_empty() || remote.port == 0 {
                return Err(anyhow::anyhow!(
                    "Invalid remote in ticket response: {:?}",
                    remote
                ));
            }
            // TODO: Currently only stream_channel_id 1 is supported
            if remote.stream_channel_id != 1 {
                return Err(anyhow::anyhow!(
                    "Invalid stream_channel_id in ticket response: {:?}",
                    remote
                ));
            }
        }
        Ok(())
    }

    pub async fn stream_channel_ids(&self) -> Vec<u16> {
        self.remotes.iter().map(|r| r.stream_channel_id).collect()
    }
}

#[derive(serde::Serialize)]
struct TicketRequest {
    token: String,
    ticket: String,
    command: String,
    ip: String,
    sent: Option<u64>,
    recv: Option<u64>,
}

impl TicketRequest {
    pub fn new_start(ticket: &Ticket, ip: &SocketAddr, auth_token: &str) -> Self {
        TicketRequest {
            token: auth_token.to_string(),
            ticket: ticket.as_str().to_string(),
            command: "start".to_string(),
            ip: ip.ip().to_string(),
            sent: None,
            recv: None,
        }
    }

    pub fn new_stop(ticket: &Ticket, auth_token: &str, sent: u64, recv: u64) -> Self {
        TicketRequest {
            token: auth_token.to_string(),
            ticket: ticket.as_str().to_string(),
            command: "stop".to_string(),
            ip: "".to_string(),
            sent: Some(sent),
            recv: Some(recv),
        }
    }
}

#[async_trait::async_trait]
pub trait BrokerApi {
    async fn start_connection(&self, ticket: &Ticket, ip: SocketAddr) -> Result<TicketResponse>;
    async fn stop_connection(&self, ticket: &Ticket) -> Result<()>;
}

pub struct HttpBrokerApi {
    client: Client,
    auth_token: String,
    ticket_rest_url: String,
}

impl HttpBrokerApi {
    pub fn new(ticket_rest_url: &str, auth_token: &str, verify_ssl: bool) -> Self {
        // Remove trailing slash if present
        let ticket_rest_url = ticket_rest_url.trim_end_matches('/');
        log::info!("Creating HttpBrokerApi with URL: {}", ticket_rest_url);
        HttpBrokerApi {
            client: Client::builder()
                .use_rustls_tls()
                .user_agent("UDSTunnelServer/5.0")
                .default_headers({
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(
                        reqwest::header::ACCEPT,
                        reqwest::header::HeaderValue::from_static("application/json"),
                    );
                    headers.insert(
                        reqwest::header::CONTENT_TYPE,
                        reqwest::header::HeaderValue::from_static("application/json"),
                    );
                    headers
                })
                .danger_accept_invalid_certs(!verify_ssl)
                .build()
                .unwrap(), // If not built, panic intentionally
            auth_token: auth_token.to_string(),
            ticket_rest_url: ticket_rest_url.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl BrokerApi for HttpBrokerApi {
    async fn start_connection(&self, ticket: &Ticket, ip: SocketAddr) -> Result<TicketResponse> {
        let ticket_request = TicketRequest::new_start(ticket, &ip, &self.auth_token);
        self.client
            .post(&self.ticket_rest_url)
            .json(&ticket_request)
            .send()
            .await?
            .error_for_status()?
            .json::<TicketResponse>()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse ticket response for ticket {}: {}",
                    ticket.as_str(),
                    e
                )
            })
    }

    async fn stop_connection(&self, ticket: &Ticket) -> Result<()> {
        // No response body expected
        let ticket_request = TicketRequest::new_stop(ticket, &self.auth_token, 0, 0);
        self.client
            .post(&self.ticket_rest_url)
            .json(&ticket_request)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to stop connection for ticket {}: {}",
                    ticket.as_str(),
                    e
                )
            })?;

        Ok(())
    }
}

pub fn get() -> impl BrokerApi {
    let config = config::get();
    let cfg = config.read().unwrap();
    HttpBrokerApi::new(
        &cfg.ticket_api_url,
        &cfg.broker_auth_token,
        cfg.verify_ssl.unwrap_or(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use mockito::Server;
    use shared::consts::TICKET_LENGTH;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    async fn setup_server_and_api(auth_token: &str) -> (mockito::ServerGuard, HttpBrokerApi) {
        log::setup_logging("debug", log::LogType::Test);

        let server = Server::new_async().await;
        let url = server.url() + "/"; // For testing, our base URL will be the mockito server

        log::info!("Setting up mock server and API client");
        let api = HttpBrokerApi::new(&url, auth_token, false);
        // Pass the base url (without /ui) to the API
        (server, api)
    }

    #[tokio::test]
    async fn test_http_broker() {
        let auth_token = "test_token";
        let (mut server, api) = setup_server_and_api(auth_token).await;
        let ticket: Ticket = [b'A'; TICKET_LENGTH].into();
        let ip = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 27, 0, 1)), 0);
        let ticket_response_json = r#"
        {
            "remotes": [
                {
                    "host": "example.com",
                    "port": 12345,
                    "stream_channel_id": 1
                }
            ],
            "notify": "notify_ticket",
            "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }
        "#;
        let _m = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ticket_response_json)
            .create();
        let response = api.start_connection(&ticket, ip).await.unwrap();
        assert_eq!(response.remotes[0].stream_channel_id, 1);
        assert_eq!(response.remotes[0].host, "example.com");
        assert_eq!(response.remotes[0].port, 12345);
        assert_eq!(response.notify, "notify_ticket");
        assert_eq!(
            *response.get_shared_secret().unwrap().as_ref(),
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
                0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
                0x89, 0xab, 0xcd, 0xef
            ]
        );
    }

    #[tokio::test]
    async fn test_http_broker_stop() {
        let auth_token = "test_token";
        let (mut server, api) = setup_server_and_api(auth_token).await;
        let ticket: Ticket = [b'A'; TICKET_LENGTH].into();
        let _m = server.mock("POST", "/").with_status(200).create();
        let result = api.stop_connection(&ticket).await;
        assert!(result.is_ok());
    }
}
