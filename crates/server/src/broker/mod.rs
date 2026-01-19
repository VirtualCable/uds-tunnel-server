use reqwest::Client;

use crate::consts::TICKET_LENGTH;

mod utils;

// @dataclasses.dataclass
// class TunnelTicketResponse:
//     """Dataclass that represents a tunnel ticket response"""

//     host: str
//     port: int
//     notify: str
//     shared_secret: str | None
#[derive(serde::Deserialize)]
pub struct TicketResponse {
    pub host: String,
    pub port: u16,
    pub notify: String,
    pub shared_secret: Option<String>,
}

#[async_trait::async_trait]
pub trait Broker {
    async fn start_connection(
        &self,
        ticket: &[u8; TICKET_LENGTH],
        ip: &str,
    ) -> reqwest::Result<TicketResponse>;
    async fn stop_connection(&self, ticket: &[u8; TICKET_LENGTH]) -> reqwest::Result<()>;
}

pub struct HttpBroker {
    client: Client,
    auth_token: String,
    ticket_rest_url: String,
}

impl HttpBroker {
    pub fn new(ticket_rest_url: &str, auth_token: &str) -> Self {
        HttpBroker {
            client: Client::new(),
            auth_token: auth_token.to_string(),
            ticket_rest_url: ticket_rest_url.to_string(),
        }
    }

    /// Note: All parameters should be already validated/encoded as needed
    pub fn get_url(&self, ticket: &[u8; TICKET_LENGTH], msg: &str) -> String {
        let ticket = String::from_utf8_lossy(ticket);
        format!(
            "{}/{}/{}/{}",
            self.ticket_rest_url, ticket, msg, self.auth_token
        )
    }
}

#[async_trait::async_trait]
impl Broker for HttpBroker {
    async fn start_connection(
        &self,
        ticket: &[u8; TICKET_LENGTH],
        ip: &str,
    ) -> reqwest::Result<TicketResponse> {
        let url = self.get_url(ticket, ip);
        let resp: TicketResponse = self
            .client
            .get(&url)
            .header("Content-Type", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp)
    }

    async fn stop_connection(&self, ticket: &[u8; TICKET_LENGTH]) -> reqwest::Result<()> {
        let url = self.get_url(ticket, "stop");
        // No response body expected
        self.client
            .delete(&url)
            .header("Content-Type", "application/json")
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }
}
