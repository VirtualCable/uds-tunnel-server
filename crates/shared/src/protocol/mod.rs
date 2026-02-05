use anyhow::Result;

mod command;
pub mod consts;
pub mod handshake;
pub mod proxy_v2;

pub use command::Command;

#[derive(Debug)]
pub struct Payload(pub Vec<u8>);

impl From<Vec<u8>> for Payload {
    fn from(value: Vec<u8>) -> Self {
        Payload(value)
    }
}

impl<const N: usize> From<&[u8; N]> for Payload {
    fn from(value: &[u8; N]) -> Self {
        Payload(value.to_vec())
    }
}

impl From<&[u8]> for Payload {
    fn from(value: &[u8]) -> Self {
        Payload(value.to_vec())
    }
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug)]
pub struct PayloadWithChannel {
    pub channel_id: u16,
    pub payload: Payload,
}

impl PayloadWithChannel {
    pub fn new(channel_id: u16, payload: &[u8]) -> Self {
        PayloadWithChannel {
            channel_id,
            payload: payload.into(),
        }
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 2 {
            anyhow::bail!("Message too short to contain channel_id");
        }
        let channel_id = u16::from_be_bytes([bytes[0], bytes[1]]);
        let payload = bytes[2..].to_vec();
        Ok(PayloadWithChannel {
            channel_id,
            payload: payload.into(),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&self.channel_id.to_be_bytes());
        data.extend_from_slice(self.payload.as_ref());
        data
    }
}

// Channel types
pub type PayloadSender = flume::Sender<Payload>;
pub type PayloadReceiver = flume::Receiver<Payload>;
pub type PayloadWithChannelSender = flume::Sender<PayloadWithChannel>;
pub type PayloadWithChannelReceiver = flume::Receiver<PayloadWithChannel>;

pub fn payload_pair() -> (PayloadSender, PayloadReceiver) {
    flume::bounded(consts::CHANNEL_SIZE)
}

pub fn payload_with_channel_pair() -> (PayloadWithChannelSender, PayloadWithChannelReceiver) {
    flume::bounded(consts::CHANNEL_SIZE)
}
