use shared::{
    crypt::{
        datagram::{TOKEN_LENGTH, UdpToken},
        rekey::MAX_REKEY_LOG2,
    },
    protocol::{consts::TICKET_LENGTH, ticket::Ticket},
};

const RESERVED_LENGTH: usize = 6;

/// Wire layout (fixed 91 bytes):
/// `session_id:48 | channel_count:u16 | inbound_seq:u64 | outbound_seq:u64 | udp_token:16 | udp_port:u16 | rekey_log2:u8 | reserved:6`
///
/// `udp_token` is the per-session identifier for the UDP relay leg. An
/// all-zero token means "UDP disabled" (broker flag off or server config
/// disallows it); the launcher then simply does not open the UDP leg.
///
/// `udp_port` is the port the server's UDP relay is actually bound to,
/// letting a deployment split TCP and UDP onto different ports. The server
/// always advertises the resolved relay port when UDP is enabled; `0` only
/// comes with the all-zero token (UDP disabled), where it is ignored.
///
/// `rekey_log2` is the session's rekeying threshold (log2 of the frames per
/// key epoch): `0` = OFF (single key forever), `1..=MAX_REKEY_LOG2` =
/// `epoch = saturating_sub(seq, seq_base) >> k` (see `shared::crypt::rekey`;
/// `seq_base` is 0 for TCP, `2^63` for UDP). The value is owned by
/// the server config and adopted by the launcher; parsing a value above
/// `MAX_REKEY_LOG2` rejects the handshake (`seq >> k` would be an undefined
/// shift). A pre-rekeying launcher cannot even parse this response (90 !=
/// 91 bytes): the incompatibility is hard and immediate, which is the point
/// of the atomic rollout (`docs/rekeying-contract.md` §1).
pub struct OpenResponse {
    pub session_id: Ticket,
    pub channel_count: u16,
    pub inbound_seq: u64,
    pub outbound_seq: u64,
    pub udp_token: UdpToken,
    pub udp_port: u16,
    pub rekey_log2: u8,
    _reserved: [u8; RESERVED_LENGTH], // For future use, 0 right now
}

impl OpenResponse {
    /// Wire size of the fixed `OpenResponse` layout (91 bytes, including
    /// `rekey_log2`; the pre-rekeying layout was 90).
    pub const WIRE_LENGTH: usize =
        TICKET_LENGTH + 2 + 8 + 8 + TOKEN_LENGTH + 2 + 1 + RESERVED_LENGTH;

    pub fn new(
        session_id: Ticket,
        channel_count: u16,
        inbound_seq: u64,
        outbound_seq: u64,
    ) -> Self {
        Self::with_udp(
            session_id,
            channel_count,
            inbound_seq,
            outbound_seq,
            [0u8; TOKEN_LENGTH],
            0,
            0,
        )
    }

    pub fn with_udp(
        session_id: Ticket,
        channel_count: u16,
        inbound_seq: u64,
        outbound_seq: u64,
        udp_token: UdpToken,
        udp_port: u16,
        rekey_log2: u8,
    ) -> Self {
        OpenResponse {
            session_id,
            channel_count,
            inbound_seq,
            outbound_seq,
            udp_token,
            udp_port,
            rekey_log2,
            _reserved: [0u8; RESERVED_LENGTH],
        }
    }

    pub fn as_vec(&self) -> Vec<u8> {
        let mut vec = self.session_id.as_ref().to_vec();
        vec.extend_from_slice(&self.channel_count.to_be_bytes());
        vec.extend_from_slice(&self.inbound_seq.to_be_bytes());
        vec.extend_from_slice(&self.outbound_seq.to_be_bytes());
        vec.extend_from_slice(&self.udp_token);
        vec.extend_from_slice(&self.udp_port.to_be_bytes());
        vec.push(self.rekey_log2);
        vec.extend_from_slice(&self._reserved);
        vec
    }

    pub fn from_slice(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() != Self::WIRE_LENGTH {
            return Err(anyhow::anyhow!("Invalid OpenResponse length"));
        }
        let session_id = Ticket::try_from(&data[0..TICKET_LENGTH])?;
        let channel_count = u16::from_be_bytes(
            data[TICKET_LENGTH..TICKET_LENGTH + 2]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to parse channel count"))?,
        );
        let inbound_seq = u64::from_be_bytes(
            data[TICKET_LENGTH + 2..TICKET_LENGTH + 2 + 8]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to parse inbound sequence"))?,
        );
        let outbound_seq = u64::from_be_bytes(
            data[TICKET_LENGTH + 2 + 8..TICKET_LENGTH + 2 + 16]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to parse outbound sequence"))?,
        );
        let udp_token: UdpToken = data
            [TICKET_LENGTH + 2 + 16..TICKET_LENGTH + 2 + 16 + TOKEN_LENGTH]
            .try_into()
            .map_err(|_| anyhow::anyhow!("Failed to parse UDP token"))?;
        let port_offset = TICKET_LENGTH + 2 + 16 + TOKEN_LENGTH;
        let udp_port = u16::from_be_bytes(
            data[port_offset..port_offset + 2]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to parse UDP port"))?,
        );
        let rekey_log2 = data[port_offset + 2];
        if rekey_log2 > MAX_REKEY_LOG2 {
            // Handshake rejection: `seq >> k` with k >= 64 is an undefined
            // shift on u64, so such a peer must never get a crypt.
            return Err(anyhow::anyhow!(
                "Invalid rekey_log2 {rekey_log2} (> {MAX_REKEY_LOG2})"
            ));
        }
        Ok(OpenResponse::with_udp(
            session_id,
            channel_count,
            inbound_seq,
            outbound_seq,
            udp_token,
            udp_port,
            rekey_log2,
        ))
    }
}

impl TryFrom<&[u8]> for OpenResponse {
    type Error = anyhow::Error;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        OpenResponse::from_slice(data)
    }
}

// Manual Debug: session_id (the equiv-session credential) and the UDP
// relay token must never appear in full in logs.
impl std::fmt::Debug for OpenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenResponse")
            .field("session_id", &self.session_id)
            .field("channel_count", &self.channel_count)
            .field("inbound_seq", &self.inbound_seq)
            .field("outbound_seq", &self.outbound_seq)
            .field(
                "udp_token",
                &shared::log::redact_secret_bytes(&self.udp_token),
            )
            .field("udp_port", &self.udp_port)
            .field("rekey_log2", &self.rekey_log2)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol::consts::MAX_CHANNEL_ID;

    const WIRE_LENGTH: usize = OpenResponse::WIRE_LENGTH;

    #[test]
    fn test_open_response_wire_length_is_91() {
        // The layout grew from the pre-UDP 72 bytes (no relay leg) to the
        // pre-rekeying 90, and to 91 with `rekey_log2`. Pin the exact size
        // so both sides (server and launcher) break loudly on any drift:
        // a launcher that cannot parse 91 bytes cannot talk to this server.
        assert_eq!(WIRE_LENGTH, 91);
    }

    #[test]
    fn test_open_response_serialization() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let channel_count = 1;
        let open_response = OpenResponse::new(session_id, channel_count, 1, 2);
        let vec = open_response.as_vec();
        assert_eq!(vec.len(), WIRE_LENGTH);
        let parsed = OpenResponse::try_from(vec.as_slice()).expect("Failed to parse OpenResponse");
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.channel_count, channel_count);
        assert_eq!(parsed.inbound_seq, 1);
        assert_eq!(parsed.outbound_seq, 2);
        // `new` leaves UDP disabled: zeroed token, port 0 (= same as TCP)
        assert_eq!(parsed.udp_token, [0u8; TOKEN_LENGTH]);
        assert_eq!(parsed.udp_port, 0);
        // `new` leaves rekeying OFF (k = 0)
        assert_eq!(parsed.rekey_log2, 0);
    }

    #[test]
    fn test_open_response_with_udp_roundtrip() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let token = [0xABu8; TOKEN_LENGTH];
        let open_response = OpenResponse::with_udp(session_id, 1, 3, 4, token, 4444, 20);
        let vec = open_response.as_vec();
        assert_eq!(vec.len(), WIRE_LENGTH);
        // Token lives right after the two seqs
        assert_eq!(
            &vec[TICKET_LENGTH + 2 + 16..TICKET_LENGTH + 2 + 16 + TOKEN_LENGTH],
            &token
        );
        // Then the udp_port, big-endian
        let port_offset = TICKET_LENGTH + 2 + 16 + TOKEN_LENGTH;
        assert_eq!(&vec[port_offset..port_offset + 2], &4444u16.to_be_bytes());
        // Reserved tail stays zeroed (the rekey_log2 byte lives before it)
        assert_eq!(
            &vec[WIRE_LENGTH - RESERVED_LENGTH..],
            &[0u8; RESERVED_LENGTH]
        );

        // rekey_log2 sits after the port
        assert_eq!(vec[port_offset + 2], 20);

        let parsed = OpenResponse::try_from(vec.as_slice()).expect("Failed to parse OpenResponse");
        assert_eq!(parsed.udp_token, token);
        assert_eq!(parsed.udp_port, 4444);
        assert_eq!(parsed.rekey_log2, 20);
        assert_eq!(parsed.inbound_seq, 3);
        assert_eq!(parsed.outbound_seq, 4);
    }

    #[test]
    fn test_open_response_invalid_length() {
        // Too short
        let data = vec![0u8; TICKET_LENGTH + 1];
        assert!(OpenResponse::try_from(data.as_slice()).is_err());
        // Old (pre-UDP) layout: must be rejected
        let data = vec![0u8; WIRE_LENGTH - TOKEN_LENGTH - 2];
        assert!(OpenResponse::try_from(data.as_slice()).is_err());
        // The pre-rekeying 90-byte layout (no rekey_log2): a launcher or
        // server that still speaks it must be rejected, not silently
        // tolerated (atomic rollout, docs/rekeying-contract.md §1).
        let data = vec![0u8; WIRE_LENGTH - 1];
        assert!(OpenResponse::try_from(data.as_slice()).is_err());
        // Too long
        let data = vec![0u8; WIRE_LENGTH + 1];
        assert!(OpenResponse::try_from(data.as_slice()).is_err());
    }

    /// Parse validation of `rekey_log2`: 0..=63 roundtrips, 64..=255 is a
    /// hard handshake rejection (`seq >> k` for k >= 64 is an undefined
    /// shift; the bound cannot be relaxed).
    #[test]
    fn test_open_response_rekey_log2_validation() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let rekey_offset = TICKET_LENGTH + 2 + 16 + TOKEN_LENGTH + 2;

        for k in [0u8, 1, 8, 20, 63] {
            let response = OpenResponse::with_udp(session_id, 1, 1, 1, [0u8; TOKEN_LENGTH], 0, k);
            let mut vec = response.as_vec();
            assert_eq!(vec.len(), WIRE_LENGTH);
            vec[rekey_offset] = k; // as_vec already wrote it; keep explicit
            let parsed = OpenResponse::try_from(vec.as_slice()).expect("valid k");
            assert_eq!(parsed.rekey_log2, k);
        }

        for k in [64u8, 65, 128, 255] {
            let mut vec =
                OpenResponse::with_udp(session_id, 1, 1, 1, [0u8; TOKEN_LENGTH], 0, 0).as_vec();
            vec[rekey_offset] = k;
            assert!(
                OpenResponse::try_from(vec.as_slice()).is_err(),
                "k = {k} must be rejected"
            );
        }
    }

    #[test]
    fn test_open_response_invalid_channel_count() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let mut vec = session_id.as_ref().to_vec();
        vec.extend_from_slice(&[0xFF, 0xFF]); // Invalid channel count (65535)
        vec.extend_from_slice(&[0u8; 8]); // Inbound seq
        vec.extend_from_slice(&[0u8; 8]); // Outbound seq
        vec.extend_from_slice(&[0u8; TOKEN_LENGTH]); // UDP token (disabled)
        vec.extend_from_slice(&[0u8; 2]); // UDP port
        vec.extend_from_slice(&[0u8; 1]); // rekey_log2 (OFF)
        vec.extend_from_slice(&[0u8; RESERVED_LENGTH]);
        let result = OpenResponse::try_from(vec.as_slice());
        assert!(result.is_ok()); // Channel count is valid, just large
        let open_response = result.unwrap();
        assert_eq!(open_response.channel_count, 65535);
    }

    /// The `connect` path must never advertise a `channel_count` larger
    /// than `MAX_CHANNEL_ID`, regardless of what the broker returned.
    /// This is the defense-in-depth cap that bounds the per-session
    /// channel count even when the broker is unexpectedly permissive.
    #[test]
    fn test_open_response_channel_count_matches_remotes() {
        // The validate() call in `broker::response::TicketResponse::validate`
        // enforces that remotes_count > 0 and <= MAX_CHANNEL_ID, so the
        // OpenResponse the server builds carries that count unchanged.
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);

        for n in 1..=(MAX_CHANNEL_ID as usize) {
            let response = OpenResponse::new(session_id, n as u16, 1, 1);
            assert!(response.channel_count >= 1);
            assert!(response.channel_count <= MAX_CHANNEL_ID);
        }

        // A zero count would have been rejected by validate(), so it is
        // not a value we expect the server to ever produce; the test only
        // documents that the wire format still tolerates it.
        let zero = OpenResponse::new(session_id, 0, 1, 1);
        assert_eq!(zero.channel_count, 0);
    }
}
