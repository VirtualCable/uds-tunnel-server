use anyhow::Result;

use super::consts;

pub struct PacketBuffer {
    buffer: [u8; consts::MAX_PACKET_SIZE as usize],
}

impl PacketBuffer {
    pub fn new() -> Self {
        PacketBuffer {
            buffer: [0u8; consts::MAX_PACKET_SIZE as usize],
        }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buffer
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buffer
    }

    pub fn ensure_capacity(&mut self, size: usize) -> Result<()> {
        if size <= consts::MAX_PACKET_SIZE as usize {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Buffer too small: {} < {}", self.buffer.len(), size))
        }
    }
}

impl Default for PacketBuffer {
    fn default() -> Self {
        Self::new()
    }
}
