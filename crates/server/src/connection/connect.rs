use std::net::SocketAddr;

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{
    crypt::types::PacketBuffer,
    errors::ErrorWithAddres,
    log,
    session::{Session, SessionManager},
    stream::{client::TunnelClientStream, server::TunnelServerStream},
    system::trigger::Trigger,
    ticket::Ticket,
};

pub(super) async fn connect<R, W>(
    mut reader: R,
    writer: W,
    ticket: &Ticket,
    ip: SocketAddr,
) -> Result<()>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    let session_manager = SessionManager::get_instance();
    match ticket.retrieve_from_broker(ip).await {
        Ok(ticket_info) => {
            let stop = Trigger::new();
            let session_id = session_manager.add_session(Session::new(
                ticket_info.get_shared_secret()?,
                *ticket,
                stop.clone(),
            ))?;
            let session = session_manager
                .get_session(&session_id)
                .ok_or(ErrorWithAddres::new(
                    Some(ip),
                    format!("Session {:?} not found", session_id).as_str(),
                ))?;

            // Store ip within session
            session.set_ip(ip);

            // Check that the first crypted packet is the ticket again
            let mut crypt = session.get_server_tunnel_crypts()?.0;

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let data: Ticket = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crypt.read(&stop, &mut reader, &mut buffer),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Timeout waiting for ticket from client: {}", e))??
            .as_slice()
            .try_into()?;

            if data != *ticket {
                log::error!("Invalid ticket from client");
                return Err(anyhow::anyhow!("Invalid ticket from client"));
            }
            // Now the recv seq should be incremented by 1 for next crypt generated
            session.set_inbound_seq(1);

            // Open a connection to the target server using the ticket info
            let target_stream = TcpStream::connect(ticket_info.target_addr().await?).await?;

            // Split the target stream into reader and writer
            let (target_reader, target_writer) = target_stream.into_split();
            // Create the tunnel streams
            let client_stream = TunnelClientStream::new(session_id, target_reader, target_writer);
            let server_stream = TunnelServerStream::new(session_id, reader, writer);

            // Run the streams concurrently
            tokio::spawn(async move {
                if let Err(e) = client_stream.run().await {
                    log::error!("Client stream error: {:?}", e);
                }
            });
            tokio::spawn(async move {
                if let Err(e) = server_stream.run().await {
                    log::error!("Server stream error: {:?}", e);
                }
            });
        }
        Err(e) => {
            log::error!("Failed to retrieve ticket info from broker: {}", e);
            return Err(anyhow::anyhow!(
                "Failed to retrieve ticket info from broker: {}",
                e
            ));
        }
    };
    Ok(())
}
