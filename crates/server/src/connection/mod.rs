#![allow(dead_code, unused_variables)]
use anyhow::Result;
use tokio::{net::TcpStream, time::Duration, time::timeout};

use crate::consts::HANDSHAKE_TIMEOUT_MS;

pub mod handshake;
pub mod proxy_v2;

pub async fn handle_connection(mut stream: TcpStream, expect_proxy_v2: bool) -> Result<()> {
    let handshake = timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        handshake::Handshake::parse(&mut stream, expect_proxy_v2),
    )
    .await?
    .map_err(|e| anyhow::anyhow!("handshake parsing error: {}", e))?;
    match handshake.action {
        handshake::HandshakeAction::Test => {
            // Just close the connection
            Ok(())
        }
        handshake::HandshakeAction::Open { ticket } => {
            // TODO: implement
            Ok(())
        }
        handshake::HandshakeAction::Recover { ticket } => {
            // TODO: implement
            Ok(())
        }
    }
}
