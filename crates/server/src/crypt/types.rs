use anyhow::Result;

use super::consts;

pub struct PacketBuffer {
    buffer: [u8; consts::MAX_PACKET_SIZE],
}

impl PacketBuffer {
    pub fn new() -> Self {
        PacketBuffer {
            buffer: [0u8; consts::MAX_PACKET_SIZE],
        }
    }

    pub fn from_slice(data: &[u8]) -> Self {
        let mut packet_buffer = PacketBuffer::new();
        let len = data.len().min(consts::MAX_PACKET_SIZE);
        packet_buffer.buffer[..len].copy_from_slice(&data[..len]);
        packet_buffer
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buffer
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
    }

    pub fn ensure_capacity(&mut self, size: usize) -> Result<()> {
        if size <= consts::MAX_PACKET_SIZE {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Buffer too small: {} < {}",
                self.buffer.len(),
                size
            ))
        }
    }

    pub fn copy_from_slice(&mut self, data: &[u8]) -> Result<()> {
        let len = data.len();
        self.ensure_capacity(len)?;
        self.buffer[..len].copy_from_slice(data);
        Ok(())
    }
}

impl Default for PacketBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl From<&[u8]> for PacketBuffer {
    fn from(data: &[u8]) -> Self {
        let mut packet_buffer = PacketBuffer::new();
        let len = data.len().min(consts::MAX_PACKET_SIZE);
        packet_buffer.buffer[..len].copy_from_slice(&data[..len]);
        packet_buffer
    }
}
