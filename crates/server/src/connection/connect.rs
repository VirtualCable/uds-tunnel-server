use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{
    crypt::{datagram::random_token, tunnel::get_udp_crypts, types::PacketBuffer},
    log,
    protocol::ticket::Ticket,
    system::trigger::Trigger,
};

use crate::{
    broker::{self, BrokerApi},
    config,
    session::{Session, SessionId, SessionManager, UdpState},
    stream::server::TunnelServerStream,
};

use super::types::OpenResponse;

/// RAII guard for a session that has been registered in the
/// [`SessionManager`] but whose connect handshake has not completed yet.
///
/// Between `add_session` and the moment the client's ticket echo is
/// validated and the `OpenResponse` is written, every early return
/// (confirm timeout, read error, invalid ticket length, content mismatch,
/// response write failure, ...) would otherwise leak a session that has
/// no owner: the proxy task never observes a stop, nothing reaps it, and
/// it occupies a slot of the `max_sessions` cap indefinitely. Half-open
/// handshakes are trivially reachable (connect, handshake, close), so a
/// leak on any of these paths is a slow resource-exhaustion DoS.
///
/// The guard removes the session on `Drop` unless [`commit`] has been
/// called, so cleanup is automatic for the current and all future error
/// paths in the handshake.
///
/// [`commit`]: PendingSession::commit
struct PendingSession {
    session: Arc<Session>,
    committed: bool,
}

impl PendingSession {
    fn new(session: Arc<Session>) -> Self {
        Self {
            session,
            committed: false,
        }
    }

    fn id(&self) -> &SessionId {
        self.session.id()
    }

    /// Mark the handshake as completed: from now on the session belongs
    /// to a validated client and must not be reaped by this guard.
    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for PendingSession {
    fn drop(&mut self) {
        if !self.committed {
            SessionManager::get_instance().remove_session(self.id());
        }
    }
}

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
            log::debug!("Received ticket info from broker: {:?}", ticket_info);
            ticket_info.validate()?; // Ensure ticket info is valid for our purposes

            // Optional per-remote-IP cap: when the config sets
            // `max_sessions_per_remote`, refuse to add the session and
            // stall the response for one second so the client cannot
            // distinguish "per-IP cap" from "broker slow" or "transient
            // network hiccup". No O(N) scan runs when the cap is
            // disabled (the default).
            //
            // Trust boundary: with `use_proxy_protocol` enabled, `src_ip`
            // comes from the PROXY v2 header and is only as trustworthy as
            // the peer that wrote it. Who may reach the server (and who
            // may speak PROXY v2 to it) is a deployment decision enforced
            // at the firewall/routing layer, exactly as for any
            // PROXY-speaking service (HAProxy, nginx, Envoy); the per-IP
            // cap is defense-in-depth, not a network boundary. Keying the
            // cap on the raw TCP peer instead would break the intended
            // posture: behind a frontend, every legitimate client shares
            // the frontend's address.
            //
            // Compute the predicate synchronously and drop the config
            // read-lock before any `.await` so the guard does not
            // cross an await point (which would break `tokio::spawn`).
            let per_remote_cap = config::get().read().unwrap().max_sessions_per_remote;
            if let Some(per_remote) = per_remote_cap
                && session_manager.count_by_remote(src_ip) >= per_remote
            {
                log::warn!(
                    "Per-remote-IP session cap hit for {} (cap {}); stalling",
                    src_ip,
                    per_remote
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                return Err(anyhow::anyhow!("session cap for remote {} reached", src_ip));
            }

            let stop = Trigger::new();
            let shared_secret = ticket_info.get_shared_secret()?;
            let session = Session::new(
                shared_secret.clone(),
                *ticket,
                stop.clone(),
                src_ip,
                ticket_info.channels_remotes(),
            );

            // UDP relay leg: only when both the broker flag and the server
            // config allow it. Keys derive from the same ticket shared
            // secret (dedicated HKDF label), so no extra handshake is
            // needed; a zero token on the OpenResponse means "disabled".
            let udp_enabled = config::get().read().unwrap().udp_enabled();
            let (udp_token, udp_port) = if ticket_info.enable_udp() && udp_enabled {
                let token = random_token();
                let (inbound, outbound) = get_udp_crypts(&shared_secret, ticket)?;
                session.set_udp(UdpState::new(token, inbound, outbound));
                log::debug!("UDP relay leg enabled for ticket {:?}", ticket);
                // Advertise the resolved UDP port so the client can reach
                // the relay even when it is split from the TCP listener.
                (token, config::get().read().unwrap().udp_sockaddr().port())
            } else {
                // A zeroed token means "UDP disabled"; the port is ignored.
                ([0u8; shared::crypt::datagram::TOKEN_LENGTH], 0)
            };

            // add_session also publishes the UDP token in the relay map.
            let session = session_manager.add_session(session)?;

            // From this point on, the registered session is owned by this
            // guard until the handshake completes; any early return below
            // removes it automatically (see `PendingSession`).
            let mut pending = PendingSession::new(session.clone());

            // Check that the first crypted packet is the ticket again
            let (mut crypt_reader, mut crypt_writer) = session.server_tunnel_crypts()?;

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let ticket_confirm = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crypt_reader.read(&mut reader, &mut buffer),
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
                // Remove the session we added: the client did not echo the ticket back,
                // so the session is unusable and must not leak in the SessionManager.
                session_manager.remove_session(session.id());
                return Err(anyhow::anyhow!("Invalid ticket from client"));
            }
            log::info!("TICKET VALIDATED");

            // Use an equivalent session id for future recovery, avoid exposing the internal session id
            let equiv_id = session_manager.create_equiv_session(session.id())?;
            // The validate() call above already enforces
            // `remotes_count <= MAX_CHANNEL_ID` and `> 0`, so the channel
            // count we advertise here matches the broker's value one-to-one
            // and the client's view of the world matches ours.
            let response = OpenResponse::with_udp(
                equiv_id,
                ticket_info.remotes_count() as u16,
                1,
                1,
                udp_token,
                udp_port,
            );
            let response_data = response.as_vec();
            // Send the OpenResponse
            crypt_writer
                .write(&mut writer, ticket_channel_id, &response_data)
                .await?;

            // Handshake completed: the session is now owned by the client
            // connection, detach it from the cleanup guard.
            pending.commit();

            log::debug!(
                "Sent OpenResponse to client with session_id: {:?}",
                response
            );

            // Sync the session seq numbers with what the handshake crypts
            // actually consumed: the inbound crypt used seq 1 for the ticket
            // echo (decrypt advances to last-used + 1, so current_seq is 2),
            // while the outbound crypt used seq 1 for the OpenResponse but
            // encrypt pre-increments (current_seq stays 1). Seeding the next
            // pair with anything less than (2, 1) on the inbound side would
            // let the anti-replay check admit a replayed copy of the
            // handshake ticket packet as the first post-handshake read;
            // keeping the outbound at 1 makes the next send seq 2, which is
            // exactly what the client expects after decrypting the
            // OpenResponse.
            session.set_seqs(crypt_reader.current_seq(), crypt_writer.current_seq());

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
