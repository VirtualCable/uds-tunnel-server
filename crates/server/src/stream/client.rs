use anyhow::Result;
use flume::{Receiver, Sender};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::{
    crypt::consts::CRYPT_PACKET_SIZE,
    log,
    session::{get_session_manager, SessionId},
    system::trigger::Trigger,
};

pub struct TunnelClientInboundStream {
    stop: Trigger,
    sender: Sender<Vec<u8>>,

    read_half: OwnedReadHalf,
}

impl TunnelClientInboundStream {
    pub fn new(read_half: OwnedReadHalf, sender: Sender<Vec<u8>>, stop: Trigger) -> Self {
        TunnelClientInboundStream {
            stop,
            sender,
            read_half,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        // Read from read_half, raw, decrypt and send to sender channel, raw
        let mut buffer = [0u8; CRYPT_PACKET_SIZE];
        loop {
            tokio::select! {
                _ = self.stop.async_wait() => {
                    break;
                }
                result = self.read_half.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            // Connection closed
                            break;
                        }
                        Ok(count) => {
                            self.sender.send_async(buffer[..count].to_vec()).await?;
                        }
                        Err(e) => {
                            return Err(anyhow::anyhow!("Client inbound read error: {:?}", e));
                        }
                    }
                }
            }
        }
        // Ensure local stop is set
        self.stop.set();
        Ok(())
    }
}

struct TunnelClientOutboundStream {
    stop: Trigger,
    receiver: Receiver<Vec<u8>>,

    write_half: OwnedWriteHalf,
}

impl TunnelClientOutboundStream {
    pub fn new(write_half: OwnedWriteHalf, receiver: Receiver<Vec<u8>>, stop: Trigger) -> Self {
        TunnelClientOutboundStream {
            stop,
            receiver,
            write_half,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.stop.async_wait() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(data) => {
                            self.write_half.write_all(&data).await?;
                        }
                        Err(_) => {
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
            // Ensure local stop is set
            self.stop.set();
        }
        Ok(())
    }
}

pub struct TunnelClientStream {
    session_id: SessionId,
    stream: TcpStream,
    inbound_channel: Receiver<Vec<u8>>,
    outbound_channel: Sender<Vec<u8>>,
}

impl TunnelClientStream {
    pub fn new(
        id: SessionId,
        stream: TcpStream,
        inbound_channel: Receiver<Vec<u8>>,
        outbound_channel: Sender<Vec<u8>>,
    ) -> Self {
        TunnelClientStream {
            session_id: id,
            stream,
            inbound_channel,
            outbound_channel,
        }
    }

    pub async fn run(self) {
        let Self {
            session_id,
            stream,
            inbound_channel,
            outbound_channel,
        } = self;

        let stop = if let Some(session) = get_session_manager().get_session(&session_id) {
            session.get_stop_trigger()
        } else {
            log::warn!("Session {:?} not found, aborting stream", session_id);
            return;
        };

        let (read_half, write_half) = stream.into_split();
        let local_stop = Trigger::new();

        let mut inbound =
            TunnelClientInboundStream::new(read_half, outbound_channel, local_stop.clone());

        let mut outbound =
            TunnelClientOutboundStream::new(write_half, inbound_channel, local_stop.clone());
        tokio::spawn(async move {
            if let Err(e) = inbound.run().await {
                log::error!("Client inbound stream error: {:?}", e);
            }
        });
        tokio::spawn(async move {
            if let Err(e) = outbound.run().await {
                log::error!("Client outbound stream error: {:?}", e);
            }
        });
        tokio::spawn(async move {
            // Notify starting client side
            get_session_manager().start_client(&session_id);
            tokio::select! {
                _ = stop.async_wait() => {
                    local_stop.set();
                }
                _ = local_stop.async_wait() => {
                    stop.set();
                }
            }
            // Notify stopping client side
            get_session_manager().stop_client(&session_id);
        });
    }
}
