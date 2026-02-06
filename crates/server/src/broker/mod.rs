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

// Authors: Adolfo Gómez, dkmaster at dkmon dot com
use std::net::SocketAddr;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use reqwest::Client;

use crate::config;
use shared::{
    crypt::{
        kem::{PRIVATE_KEY_SIZE, PUBLIC_KEY_SIZE, comms_keypair},
        ticket::EncryptedTicketResponse,
    },
    log,
    protocol::ticket::Ticket,
};

mod request;
mod response;

#[async_trait::async_trait]
pub trait BrokerApi {
    async fn start_connection(
        &self,
        ticket: &Ticket,
        ip: SocketAddr,
    ) -> Result<response::TicketResponse>;
    async fn stop_connection(&self, ticket: &Ticket) -> Result<()>;
}

pub struct HttpBrokerApi {
    client: Client,
    auth_token: String,
    ticket_rest_url: String,
    public_key: [u8; PUBLIC_KEY_SIZE],
    private_key: [u8; PRIVATE_KEY_SIZE],
}

impl HttpBrokerApi {
    pub fn new(ticket_rest_url: &str, auth_token: &str, verify_ssl: bool) -> Self {
        // Remove trailing slash if present
        let ticket_rest_url = ticket_rest_url.trim_end_matches('/');
        log::info!("Creating HttpBrokerApi with URL: {}", ticket_rest_url);
        let keys = comms_keypair();
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
            public_key: keys.public_key,
            private_key: keys.private_key,
        }
    }

    // Only for tests
    #[cfg(test)]
    pub fn with_keys(
        self,
        private_key: [u8; PRIVATE_KEY_SIZE],
        public_key: [u8; PUBLIC_KEY_SIZE],
    ) -> Self {
        Self {
            public_key,
            private_key,
            ..self
        }
    }
}

#[async_trait::async_trait]
impl BrokerApi for HttpBrokerApi {
    async fn start_connection(
        &self,
        ticket: &Ticket,
        ip: SocketAddr,
    ) -> Result<response::TicketResponse> {
        let ticket_request = request::TicketRequest::new_start(
            ticket,
            &ip,
            &self.auth_token,
            general_purpose::STANDARD.encode(self.public_key),
        );
        self.client
            .post(&self.ticket_rest_url)
            .json(&ticket_request)
            .send()
            .await?
            .error_for_status()?
            .json::<EncryptedTicketResponse>()
            .await?
            .recover_data_from_json(ticket, &self.private_key)
            .map(|json_value| {
                serde_json::from_value::<response::TicketResponse>(json_value)
                    .map_err(|e| anyhow::format_err!("Failed to parse ticket response JSON: {}", e))
            })?
    }

    async fn stop_connection(&self, ticket: &Ticket) -> Result<()> {
        // No response body expected
        let ticket_request = request::TicketRequest::new_stop(ticket, &self.auth_token, 0, 0);
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

// pub because constants are used elsewhere
#[cfg(test)]
pub mod tests;
