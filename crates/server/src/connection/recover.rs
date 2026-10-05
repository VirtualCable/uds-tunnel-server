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
            // the `in_seqs.0 - 1` subtraction below. The full window validation
            // (against the session's outbound counter and the buffered window)
            // runs only after the peer authenticates, below.
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
                // Judge the declaration against the session's authoritative
                // outbound counter, not only the surviving buffer window.
                // Two legitimate shapes used to be rejected outright:
                //  - a leg that dropped after the `OpenResponse` and before
                //    the first buffered data frame: the window is empty
                //    (`tail == 0`) while the launcher's last received seq is
                //    exactly the last seq the session encrypted. Refusing it
                //    lost a recoverable session.
                //  - `requested == head - 1`: the launcher acknowledged none
                //    of the buffered window, which simply means "re-send all
                //    of it", not a malformed recovery.
                // Malformed shapes: a declaration above everything the
                // session ever encrypted (fabricated), and a declaration
                // below the retained window whose gap frames were neither
                // buffered nor acknowledged (an eviction hole — no
                // contiguous retransmission is possible).
                let last_sent = std::cmp::max(tail, session.seqs().1);
                if requested > last_sent {
                    log::error!(
                        "Refusing recovery for session {:?}: requested seq {} exceeds last sent seq {} (buffer window [{}, {}])",
                        session.id(),
                        requested,
                        last_sent,
                        head,
                        tail
                    );
                    return Err(anyhow::anyhow!(
                        "Invalid recovery sequence: requested seq beyond last sent seq"
                    ));
                }
                if head != 0 && requested < head - 1 {
                    log::error!(
                        "Refusing recovery for session {:?}: requested seq {} below retained window [{}, {}] (evicted gap)",
                        session.id(),
                        requested,
                        head,
                        tail
                    );
                    return Err(anyhow::anyhow!(
                        "Invalid recovery sequence: requested seq below recovery buffer window"
                    ));
                }
                // Skip only into the retained window. At or below `head - 1`
                // the whole window is due for retransmission and must stay
                // intact — `skip` walks from the front and would drain it.
                // A `NotFound` here means `requested` is above the retained
                // `tail` (everything buffered was acknowledged, the remaining
                // declared frames were encrypted but not yet on the wire at
                // snapshot time): the drain consumed the full window, which
                // is exactly the correct outcome, so it is not an error.
                if head != 0 && requested >= head {
                    let _ = rbuf.skip(requested);
                    log::debug!(
                        "Skipped packets until seq {:?} for session {:?} recovery (buf: {:?})",
                        requested,
                        session.id(),
                        rbuf,
                    );
                }
            }
            // Adopt the recovering peer's address: the AEAD confirm above
            // proves this socket is the session's launcher, so it is the
            // session's source address from here on. Skipping the update
            // left a moved client (a VPN re-bind, a NAT that changes the
            // public address, a laptop that switched networks) pinned to
            // the address it opened with: the per-IP session cap kept
            // counting it against the old address, and the UDP relay's
            // foreign-source check (`src.ip() == session.src_ip().ip()`)
            // refused to re-point the return path at the new one, so the
            // UDP leg died silently while the TCP tunnel worked.
            session.set_ip(ip);
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
                    crate::config::get()
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .udp_sockaddr()
                        .port(),
                ),
                None => ([0u8; shared::crypt::datagram::TOKEN_LENGTH], 0),
            };
            let response =
                // `rekey_log2` is re-advertised from the session, never
                // re-read from the config: a Recover must not renegotiate
                // the key epoching of a live session (docs/rekeying-contract.md
                // §4). The launcher already knows `k` from its Open; the
                // crypts above were rebuilt from `session.k` too.
                OpenResponse::with_udp(
                    equiv_id,
                    0,
                    in_seq,
                    out_seq,
                    udp_token,
                    udp_port,
                    session.rekey_log2(),
                ); // On recover, no new streams are created
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
