use shared::ticket::Ticket;

const RESERVED_LENGTH: usize = 6;

pub struct OpenResponse {
    pub session_id: Ticket,
    pub channel_count: u16,              // 1 right now
    pub reserved: [u8; RESERVED_LENGTH], // For future use, 0 right now
}

impl OpenResponse {
    pub fn new(session_id: Ticket, channel_count: u16) -> Self {
        OpenResponse {
            session_id,
            channel_count,
            reserved: [0u8; RESERVED_LENGTH],
        }
    }

    pub fn as_vec(&self) -> Vec<u8> {
        let mut vec = self.session_id.as_ref().to_vec();
        vec.extend_from_slice(&self.channel_count.to_be_bytes());
        vec.extend_from_slice(&self.reserved);
        vec
    }

}
