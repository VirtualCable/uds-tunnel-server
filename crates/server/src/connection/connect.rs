use std::net::SocketAddr;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{errors::ErrorWithAddres, log, ticket::Ticket};

pub(super) async fn connect<R, W>(
    mut reader: R,
    mut writer: W,
    ticket: &Ticket,
    ip: SocketAddr,
) -> Result<(), ErrorWithAddres>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    match ticket.retrieve_from_broker(ip).await {
        Ok(ticket_info) => {
            log::info!("Ticket info retrieved: {:?}", ticket_info);
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to retrieve ticket info from broker: {}", e);
            Err(ErrorWithAddres::new(
                Some(ip),
                format!("Failed to retrieve ticket info from broker: {}", e).as_str(),
            ))
        }
    }
}
