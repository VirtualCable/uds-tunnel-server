#![allow(dead_code)]
// TODO: Remove allow dead_code when the module is fully implemented

use anyhow::Result;
use flume::{Receiver, Sender};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::{crypt::consts::CRYPT_PACKET_SIZE, log, system::trigger::Trigger};

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
        }
        Ok(())
    }
}

pub struct TunnelClientStream {
    stream: TcpStream,
    inbound_channel: Receiver<Vec<u8>>,
    outbound_channel: Sender<Vec<u8>>,
    stop: Trigger,
}

impl TunnelClientStream {
    pub fn new(
        stream: TcpStream,
        inbound_channel: Receiver<Vec<u8>>,
        outbound_channel: Sender<Vec<u8>>,
        stop: Trigger,
    ) -> Self {
        TunnelClientStream {
            stream,
            inbound_channel,
            outbound_channel,
            stop,
        }
    }

    pub async fn run(self) {
        let Self {
            stream,
            inbound_channel,
            outbound_channel,
            stop,
        } = self;

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
            stop.async_wait().await;
            local_stop.set();
        });
    }
}
