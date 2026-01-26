use std::net::SocketAddr;

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use shared::{crypt::types::PacketBuffer, log, system::trigger::Trigger, ticket::Ticket};

use crate::{
    broker::{self, BrokerApi},
    session::{Session, SessionManager},
    stream::{client::TunnelClientStream, server::TunnelServerStream},
};

pub(super) async fn connect<R, W>(
    mut reader: R,
    mut writer: W,
    ticket: &Ticket,
    ip: SocketAddr,
) -> Result<()>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    let session_manager = SessionManager::get_instance();
    let broker = broker::get();
    match broker.start_connection(ticket, ip).await {
        // Note: On a future, the broker could return more than a single channel stream id
        // But currently, only one is supported (1)
        Ok(ticket_info) => {
            let stop = Trigger::new();
            let (session_id, session) = session_manager.add_session(Session::new(
                ticket_info.get_shared_secret()?,
                *ticket,
                1,  // only channel 1 is supported for now (0 is for commands)
                stop.clone(),
                ip,
            ))?;

            // Check that the first crypted packet is the ticket again
            let (mut read_crypt, mut write_crypt) = session.server_tunnel_crypts()?;

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let ticket_data = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                read_crypt.read(&stop, &mut reader, &mut buffer),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Timeout waiting for ticket from client: {}", e))?;

            // If reading ticket data failed, ensure session is removed and return error
            let (data, channel) : (Ticket, u16)  = if let Ok((bytes, channel)) = ticket_data {
                (bytes.try_into()?, channel)
            } else {
                log::error!("Failed to read ticket data from client");
                // Remove the session, that has not been used properly
                session_manager.remove_session(&session_id);
                return Err(anyhow::anyhow!("Failed to read ticket data from client"));
            };
            
            // TODO: Check channel?
            if data != *ticket {
                log::error!("Invalid ticket from client");
                return Err(anyhow::anyhow!("Invalid ticket from client"));
            }
            // Sent the sessionId as response, so the client can verify
            let sess_id = session_id.as_str().as_bytes();
            write_crypt.write(&mut writer, channel,  sess_id).await?;

            // Now the recv/send seq should be set to 1 for next crypt managers
            // (we already spent seq 0 for ticket exchange)
            session.set_inbound_seq(1);
            session.set_outbound_seq(1);

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
