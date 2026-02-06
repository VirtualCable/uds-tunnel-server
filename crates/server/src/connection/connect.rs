use std::net::SocketAddr;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{crypt::types::PacketBuffer, log, protocol::ticket::Ticket, system::trigger::Trigger};

use crate::{
    broker::{self, BrokerApi},
    session::{Session, SessionManager},
    stream::server::TunnelServerStream,
};

use super::types::OpenResponse;

pub(super) async fn connect<R, W>(
    mut reader: R,
    mut writer: W,
    ticket: &Ticket,
    src_ip: SocketAddr,
) -> Result<()>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    let session_manager = SessionManager::get_instance();
    let broker = broker::get();
    match broker.start_connection(ticket, src_ip).await {
        // Note: On a future, the broker could return more than a single channel stream id
        // But currently, only one is supported, althout it's prepared to be extended later
        Ok(ticket_info) => {
            ticket_info.validate()?; // Ensure ticket info is valid for our purposes

            let stop = Trigger::new();
            let session = session_manager.add_session(Session::new(
                ticket_info.get_shared_secret()?,
                *ticket,
                stop.clone(),
                src_ip,
                ticket_info.channels_remotes(),
            ))?;

            // Check that the first crypted packet is the ticket again
            let (mut crypt_reader, mut crypt_writer) = session.server_tunnel_crypts()?;

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let ticket_confirm = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crypt_reader.read(&stop, &mut reader, &mut buffer),
            )
            .await
            .map_err(|e| anyhow::anyhow!("Timeout waiting for ticket from client: {}", e))?;

            // If reading ticket data failed, ensure session is removed and return error
            let (data, ticket_channel_id): (Ticket, u16) =
                if let Ok((bytes, channel_id)) = ticket_confirm {
                    (bytes.try_into()?, channel_id)
                } else {
                    log::error!("Failed to read ticket data from client");
                    // Remove the session, that has not been used properly
                    session_manager.remove_session(session.id());
                    return Err(anyhow::anyhow!("Failed to read ticket data from client"));
                };

            // Channel does not matter here in fact, just extract the data. This is a MUST match
            if data != *ticket {
                log::error!("Invalid ticket from client");
                return Err(anyhow::anyhow!("Invalid ticket from client"));
            }

            // Now the recv/send seq should be set to 1 for next crypt managers
            // (we already spent seq 0 for ticket exchange)
            // In fact, we spent seq 1, because the crypt is pre-incrementing before use
            // So next expected seq is 2 on both sides.
            session.set_inbound_seq(1);
            session.set_outbound_seq(1);

            // for remote_index in 0..ticket_info.get_remotes_count() {
            //     let target_addr = ticket_info.target_addr(remote_index as u16).await?;
            //     log::info!(
            //         "Established connection to remote target {} for session {:?}",
            //         target_addr,
            //         session.id()
            //     );
            //     // Open a connection to the target server using the ticket info
            //     let target_stream =
            //         TcpStream::connect(ticket_info.target_addr(remote_index as u16).await?).await?;

            //     // Split the target stream into reader and writer
            //     let (target_reader, target_writer) = target_stream.into_split();

            //     let client_stream = TunnelClientStream::new(
            //         *session.id(),
            //         (remote_index + 1) as u16,
            //         target_reader,
            //         target_writer,
            //     );
            //     // Run the streams concurrently
            //     tokio::spawn(async move {
            //         if let Err(e) = client_stream.run().await {
            //             log::error!("Client stream error: {:?}", e);
            //         }
            //     });
            // }

            // Use an equivalent session id for future recovery, avoid exposing the internal session id
            let equiv_id = session_manager.create_equiv_session(session.id())?;
            let response = OpenResponse::new(equiv_id, ticket_info.remotes_count() as u16);
            let response_data = response.as_vec();
            // Send the OpenResponse
            crypt_writer
                .write(&mut writer, ticket_channel_id, &response_data)
                .await?;

            // Server stream is the one connected to the client
            let server_stream = TunnelServerStream::new(*session.id(), reader, writer);
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
