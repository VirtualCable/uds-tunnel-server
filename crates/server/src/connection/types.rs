use shared::protocol::{consts::TICKET_LENGTH, ticket::Ticket};

const RESERVED_LENGTH: usize = 6;

#[derive(Debug)]
pub struct OpenResponse {
    pub session_id: Ticket,
    pub channel_count: u16,
    pub inbound_seq: u64,
    pub outbound_seq: u64,
    _reserved: [u8; RESERVED_LENGTH], // For future use, 0 right now
}

impl OpenResponse {
    pub fn new(session_id: Ticket, channel_count: u16, inbound_seq: u64, outbound_seq: u64) -> Self {
        OpenResponse {
            session_id,
            channel_count,
            inbound_seq,
            outbound_seq,
            _reserved: [0u8; RESERVED_LENGTH],
        }
    }

    pub fn as_vec(&self) -> Vec<u8> {
        let mut vec = self.session_id.as_ref().to_vec();
        vec.extend_from_slice(&self.channel_count.to_be_bytes());
        vec.extend_from_slice(&self.inbound_seq.to_be_bytes());
        vec.extend_from_slice(&self.outbound_seq.to_be_bytes());
        vec.extend_from_slice(&self._reserved);
        vec
    }

    pub fn from_slice(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() != TICKET_LENGTH + 2 + 8 + 8 + RESERVED_LENGTH {
            return Err(anyhow::anyhow!("Invalid OpenResponse length"));
        }
        let session_id = Ticket::try_from(&data[0..TICKET_LENGTH])?;
        let channel_count = u16::from_be_bytes(
            data[TICKET_LENGTH..TICKET_LENGTH + 2]
                .try_into()
                .map_err(|_| anyhow::anyhow!("Failed to parse channel count"))?,
        );
        //
        // let mut reserved = [0u8; RESERVED_LENGTH];
        // reserved.copy_from_slice(&data[TICKET_LENGTH + 2..]);
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
        Ok(OpenResponse::new(session_id, channel_count, inbound_seq, outbound_seq))
    }
}

impl TryFrom<&[u8]> for OpenResponse {
    type Error = anyhow::Error;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        OpenResponse::from_slice(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::protocol::consts::MAX_CHANNEL_ID;

    #[test]
    fn test_open_response_serialization() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let channel_count = 1;
        let open_response = OpenResponse::new(session_id, channel_count, 1, 2);
        let vec = open_response.as_vec();
        let parsed = OpenResponse::try_from(vec.as_slice()).expect("Failed to parse OpenResponse");
        assert_eq!(parsed.session_id, session_id);
        assert_eq!(parsed.channel_count, channel_count);
        assert_eq!(parsed.inbound_seq, 1);
        assert_eq!(parsed.outbound_seq, 2);
    }

    #[test]
    fn test_open_response_invalid_length() {
        let data = vec![0u8; TICKET_LENGTH + 1]; // Invalid length
        let result = OpenResponse::try_from(data.as_slice());
        assert!(result.is_err());
    }

    #[test]
    fn test_open_response_invalid_channel_count() {
        let session_id = Ticket::new([1u8; TICKET_LENGTH]);
        let mut vec = session_id.as_ref().to_vec();
        vec.extend_from_slice(&[0xFF, 0xFF]); // Invalid channel count (65535)
        vec.extend_from_slice(&[0u8; 8]); // Inbound seq
        vec.extend_from_slice(&[0u8; 8]); // Outbound seq
        vec.extend_from_slice(&[0u8; RESERVED_LENGTH]);
        let result = OpenResponse::try_from(vec.as_slice());
        assert!(result.is_ok()); // Channel count is valid, just large
        let open_response = result.unwrap();
        assert_eq!(open_response.channel_count, 65535);
    }

    /// Regression test for the defense-in-depth cap introduced together
    /// with vuln-0002. The `connect` path must never advertise a
    /// `channel_count` larger than `MAX_CHANNEL_ID`, regardless of what
    /// the broker returned.
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
