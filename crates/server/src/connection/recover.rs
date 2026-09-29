use std::net::SocketAddr;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use shared::{crypt::types::PacketBuffer, log, protocol::ticket::Ticket};

use crate::{
    consts::HANDSHAKE_CONFIRM_TIMEOUT_SECS, session::SessionManager,
    stream::server::TunnelServerStream,
};

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
            // Validate the client-supplied sequence numbers BEFORE touching the
            // recovery buffer. Crypt::next_seq pre-increments from 0, so the
            // smallest legitimate inbound seq is 1. A value of 0 would underflow
            // the `in_seqs.0 - 1` subtraction below. Any value outside the
            // buffer's [head, tail] window is malformed and must be rejected
            // without emptying the buffer.
            if in_seqs.0 == 0 {
                log::error!(
                    "Refusing recovery for session {:?}: in_seq underflows",
                    session.id()
                );
                return Err(anyhow::anyhow!(
                    "Invalid recovery sequence: in_seq must be >= 1"
                ));
            }
            // NOTE: the request-driven session effects — consuming
            // the recovery-buffer retransmission window and holding the attach
            // lock — happen only AFTER the AEAD ticket confirm authenticates
            // the peer below. Knowing the equiv session id alone must not
            // drain the window nor occupy the lock.
            let session_id = session.id();
            // Crypts sharing the session counters: decrypting the ticket
            // confirm and encrypting the OpenResponse advance the session's
            // authoritative seq numbers directly, atomically with their use.
            let (mut crypt_reader, mut crypt_writer) = session.server_tunnel_crypts()?;

            // Snapshot the counters *before* reading the ticket confirm: the
            // confirm is itself one inbound frame, so the sequence pair the
            // launcher's new stream resumes from is the pre-confirm value.
            let (in_seq, out_seq) = session.seqs();

            let mut buffer: PacketBuffer = PacketBuffer::new();
            let rec_sessid_confirm = tokio::time::timeout(
                std::time::Duration::from_secs(HANDSHAKE_CONFIRM_TIMEOUT_SECS),
                crypt_reader.read(&mut reader, &mut buffer),
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("Timeout waiting for recover session id from client: {}", e)
            })?;

            // If reading ticket data failed, ensure session is removed and return error.
            // This teardown is DELIBERATE, not an oversight: the leg is AEAD-sealed, so
            // a frame that fails to decrypt cannot be a transient network artifact — it
            // comes from a malicious peer (one that captured the equiv ticket but has
            // no way to produce a valid crypt) or from a buggy client, and both cases
            // are treated the same. A merely slow client takes the timeout path above,
            // which leaves the session intact. Killing the session is the safe response
            // to a cryptographically invalid packet; do not relax it into a plain early
            // return.
            let (data, stream_channel_id): (Ticket, u16) =
                if let Ok((bytes, channel_id)) = rec_sessid_confirm {
                    (bytes.try_into()?, channel_id)
                } else {
                    log::error!("Failed to read ticket data from client");
                    // Remove the session: a malformed crypted packet here proves the
                    // peer is not the legitimate client (see rationale above).
                    session_manager.remove_session(session_id);
                    return Err(anyhow::anyhow!("Failed to read ticket data from client"));
                };

            // Channel does not matter here in fact, just extract the data. This is a MUST match
            if data != *recover_session_id {
                log::error!("Invalid recover session id from client");
                return Err(anyhow::anyhow!("Invalid recover session id from client"));
            }
            // The peer is authenticated at this point: it produced a valid
            // AEAD ticket confirm over a leg whose keys only the ticket
            // holder possesses. Only now may its request drive
            // session state. Take the attach lock first so two concurrent
            // Recovers cannot interleave the window consumption with the
            // attach swap, then consume the recovery window it declared.
            //
            // The lock is held for the rest of the handshake (the attach
            // swap at `start_server` below), serializing this Recover
            // against a second concurrent Recover, a fresh Open attach, and
            // the dying stream's teardown. The still-live stream is NOT
            // killed before the confirm: a timed-out or forged Recover
            // leaves the existing tunnel untouched; `start_server` performs
            // the kill+attach atomically now that the confirm has been
            // accepted.
            let _attach_guard = session.lock_server_attach().await;
            {
                let ses_rec_buf = session.recovery_buffer();
                let mut rbuf = ses_rec_buf.lock();
                log::debug!(
                    "Found session {:?} for recovery, skipping packets until seq {:?} (buf: {:?})",
                    session.id(),
                    in_seqs.0,
                    rbuf
                );
                let requested = in_seqs.0 - 1;
                let head = rbuf.head_seq();
                let tail = rbuf.tail_seq();
                // Empty buffer (head == tail == 0) means the server has nothing
                // buffered for retransmission: a legitimate client always asks
                // for seq >= 1, so any non-empty request against an empty
                // window is also a malformed recovery attempt.
                if tail == 0 || requested < head || requested > tail {
                    log::error!(
                        "Refusing recovery for session {:?}: requested seq {} outside buffer window [{}, {}]",
                        session.id(),
                        in_seqs.0,
                        head,
                        tail
                    );
                    return Err(anyhow::anyhow!(
                        "Invalid recovery sequence: requested seq outside recovery buffer window"
                    ));
                }
                rbuf.skip(requested)?;
                log::debug!(
                    "Skipped packets until seq {:?} for session {:?} recovery (buf: {:?})",
                    requested,
                    session.id(),
                    rbuf,
                );
            }
            // Invalidate the old equiv session ID before minting a new
            // one: the id the peer just used must stop resolving the
            // moment recovery succeeds, so a stolen or replayed
            // recovery credential cannot keep pointing at the session.
            session_manager.remove_equiv_session(recover_session_id);
            let equiv_id = session_manager.create_equiv_session(session_id)?;
            // The UDP leg survives recovery: keys derive from the ticket
            // and do not change, so the same token goes back out. A zero
            // token means the session never had UDP enabled.
            let (udp_token, udp_port) = match session.udp() {
                Some(udp) => (
                    udp.token,
                    crate::config::get().read().unwrap().udp_sockaddr().port(),
                ),
                None => ([0u8; shared::crypt::datagram::TOKEN_LENGTH], 0),
            };
            let response =
                OpenResponse::with_udp(equiv_id, 0, in_seq, out_seq, udp_token, udp_port); // On recover, no new streams are created
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

            // The ticket confirm advanced the shared inbound counter past the
            // confirm frame and the OpenResponse write consumed the next
            // outbound seq, so the new stream's crypts (rebuilt from the
            // session counters) resume strictly after the handshake without
            // any explicit reservation.
            //
            // Attach the new launcher-facing stream while still holding the
            // attach lock: start_server kills the previous owner, allocates
            // the proxy channel set and registers the new owner, and any
            // later attach can only see the *new* stream. Keeping the kill
            // and the attach in one critical section is what prevents a
            // second concurrent Recover from interleaving with this handshake.
            let (endpoints, owner) = session.start_server().await?;

            let server_stream = TunnelServerStream::new(*session_id, reader, writer);
            server_stream.run_attached(session.clone(), endpoints, owner);
        }
        None => {
            log::error!("Failed to retrieve recover session id");
            return Err(anyhow::anyhow!("Failed to retrieve recover session id"));
        }
    };
    Ok(())
}
