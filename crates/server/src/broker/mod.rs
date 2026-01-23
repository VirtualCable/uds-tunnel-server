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
pub struct TicketResponse {
    pub host: String,
    pub port: u16,
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

    pub async fn target_addr(&self) -> Result<SocketAddr> {
        let resolver = Resolver::builder_with_config(
            ResolverConfig::default(),
            TokioConnectionProvider::default(),
        )
        .build();

        match resolver.lookup_ip(&self.host).await {
            Ok(lookup) => {
                let ip = lookup.iter().next().ok_or_else(|| {
                    anyhow::anyhow!("No IP addresses found for host: {}", self.host)
                })?;
                Ok(SocketAddr::new(ip, self.port))
            }
            Err(e) => Err(anyhow::anyhow!(
                "DNS resolution failed for {}: {}",
                self.host,
                e
            )),
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

    /// Note: All parameters should be already validated/encoded as needed
    pub fn get_url(&self, ticket: &Ticket, msg: &str) -> String {
        format!(
            "{}/{}/{}/{}",
            self.ticket_rest_url,
            ticket.as_str(),
            msg,
            self.auth_token
        )
    }
}

#[async_trait::async_trait]
impl BrokerApi for HttpBrokerApi {
    async fn start_connection(&self, ticket: &Ticket, ip: SocketAddr) -> Result<TicketResponse> {
        let url = self.get_url(ticket, &ip.ip().to_string());
        let resp: TicketResponse = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse TicketResponse from broker for ticket {}: {}",
                    ticket.as_str(),
                    e
                )
            })?;
        Ok(resp)
    }

    async fn stop_connection(&self, ticket: &Ticket) -> Result<()> {
        let url = self.get_url(ticket, "stop");
        // No response body expected
        self.client
            .delete(&url)
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

    use shared::consts::TICKET_LENGTH;
    use mockito::Server;
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
            "host": "example.com",
            "port": 12345,
            "notify": "notify_ticket",
            "shared_secret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        }
        "#;
        let _m = server
            .mock(
                "GET",
                format!(
                    "/{}/{}/{}",
                    ticket.as_str(),
                    ip.ip().to_string().as_str(),
                    auth_token
                )
                .as_str(),
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ticket_response_json)
            .create();
        let response = api.start_connection(&ticket, ip).await.unwrap();
        assert_eq!(response.host, "example.com");
        assert_eq!(response.port, 12345);
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
        let _m = server
            .mock(
                "DELETE",
                format!("/{}/stop/{}", ticket.as_str(), auth_token).as_str(),
            )
            .with_status(200)
            .create();
        let result = api.stop_connection(&ticket).await;
        assert!(result.is_ok());
    }
}
