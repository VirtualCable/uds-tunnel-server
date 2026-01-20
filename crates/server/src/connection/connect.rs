use std::net::SocketAddr;

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{
    errors::ErrorWithAddres,
    log,
    stream::{client::TunnelClientStream, server::TunnelServerStream},
    session::{SessionManager, SessionId},
    ticket::Ticket,
};

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
            // Open a connection to the target server using the ticket info
            let mut target_stream = TcpStream::connect(ticket_info.target_addr())
                .await
                .map_err(|e| {
                    log::error!("Failed to connect to target server: {}", e);
                    ErrorWithAddres::new(
                        Some(ip),
                        format!("Failed to connect to target server: {}", e).as_str(),
                    )
                })?;
            
            // Split the target stream into reader and writer
            let (mut target_reader, mut target_writer) = target_stream.split();
            // Create the tunnel streams
            // let mut client_stream = TunnelClientStream::new(&mut target_reader, &mut target_writer);
            // let mut server_stream = TunnelServerStream::new(&mut reader, &mut writer);

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
