use std::net::SocketAddr;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{crypt::types::PacketBuffer, log, protocol::ticket::Ticket, system::trigger::Trigger};

use crate::{session::SessionManager, stream::server::TunnelServerStream};

use super::types::OpenResponse;

pub(super) async fn recover<R, W>(
    mut reader: R,
    mut writer: W,
    recover_session_id: &Ticket,
    in_seqs: (u64, u64),
    ip: SocketAddr,
) -> Result<()>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    log::debug!(
        "Attempting to recover session {:?}-{:?} from client {:?}",
        recover_session_id,
        in_seqs,
        ip
    );

    let session_manager = SessionManager::get_instance();
    match session_manager.get_equiv_session(recover_session_id) {
        Some(session) => {
            // Skip packages in the recovery buffer until the requested seq is found, if not found, return error
            {
                let ses_rec_buf = session.recovery_buffer();
                let buffer = ses_rec_buf.get();
                log::debug!(
                    "Found session {:?} for recovery, skipping packets until seq {:?} (buf: {:?})",
                    session.id(),
                    in_seqs.0,
                    buffer
                );
                buffer.skip(in_seqs.0 - 1)?;
                log::debug!(
                    "Skipped packets until seq {:?} for session {:?} recovery (buf: {:?})",
                    in_seqs.0 - 1,
                    session.id(),
                    buffer,
                );
            }
            let stop = Trigger::new();
            let session_id = session.id();
            // Check that the first crypted packet is the ticket again
            let (mut crypt_reader, mut crypt_writer) = session.server_tunnel_crypts()?;

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let rec_sessid_confirm = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crypt_reader.read(&mut reader, &mut buffer),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("Timeout waiting for recover session id from client: {}", e)
            })?;

            // If reading ticket data failed, ensure session is removed and return error
            let (data, stream_channel_id): (Ticket, u16) =
                if let Ok((bytes, channel_id)) = rec_sessid_confirm {
                    (bytes.try_into()?, channel_id)
                } else {
                    log::error!("Failed to read ticket data from client");
                    // Remove the session, that has not been used properly
                    session_manager.remove_session(session_id);
                    return Err(anyhow::anyhow!("Failed to read ticket data from client"));
                };

            // Channel does not matter here in fact, just extract the data. This is a MUST match
            if data != *recover_session_id {
                log::error!("Invalid recover session id from client");
                return Err(anyhow::anyhow!("Invalid recover session id from client"));
            }
            let equiv_id = session_manager.create_equiv_session(session_id)?;
            let (in_seq, out_seq) = session.seqs();
            let response = OpenResponse::new(equiv_id, 0, in_seq, out_seq); // On recover, no new streams are created
            let response_data = response.as_vec();
            log::debug!(
                "Recovering session {:?} for client {:?}, sending OpenResponse {:?}",
                session_id,
                ip,
                response
            );
            // Send the OpenResponse
            crypt_writer
                .write(&mut writer, stream_channel_id, &response_data)
                .await?;

            // Now the recv/send seq should have been keept from previous session, increment them both by 1
            // because we just received and sent one packet each
            session.set_inbound_seq(in_seq + 1);
            session.set_outbound_seq(out_seq + 1);
            // Note: both tunnel sides will create a crypt based on these seq numbers

            // Client stream should already be there, just create the server stream
            let server_stream = TunnelServerStream::new(*session_id, reader, writer);

            tokio::spawn(async move {
                if let Err(e) = server_stream.run().await {
                    log::error!("Server stream error: {:?}", e);
                }
            });
        }
        None => {
            log::error!("Failed to retrieve recover session id");
            return Err(anyhow::anyhow!("Failed to retrieve recover session id"));
        }
    };
    Ok(())
}
