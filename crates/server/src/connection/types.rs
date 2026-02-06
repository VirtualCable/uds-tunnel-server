use shared::protocol::{consts::TICKET_LENGTH, ticket::Ticket};

const RESERVED_LENGTH: usize = 6;

pub struct OpenResponse {
    pub session_id: Ticket,
    pub channel_count: u16,           // 1 right now
    _reserved: [u8; RESERVED_LENGTH], // For future use, 0 right now
}

impl OpenResponse {
    pub fn new(session_id: Ticket, channel_count: u16) -> Self {
        OpenResponse {
            session_id,
            channel_count,
            _reserved: [0u8; RESERVED_LENGTH],
        }
    }

    pub fn as_vec(&self) -> Vec<u8> {
        let mut vec = self.session_id.as_ref().to_vec();
        vec.extend_from_slice(&self.channel_count.to_be_bytes());
        vec.extend_from_slice(&self._reserved);
        vec
    }

    pub fn from_slice(data: &[u8]) -> anyhow::Result<Self> {
        if data.len() != TICKET_LENGTH + 2 + RESERVED_LENGTH {
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
        Ok(OpenResponse::new(session_id, channel_count))
    }
}
