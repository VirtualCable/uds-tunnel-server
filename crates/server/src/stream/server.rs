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

use crate::{
    crypt::{
        Crypt, build_header,
        consts::{CRYPT_PACKET_SIZE, HEADER_LENGTH},
        parse_header,
        types::PacketBuffer,
    },
    log,
    system::trigger::Trigger,
};

struct TunnelServerInboundStream {
    stop: Trigger,
    sender: Sender<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    read_half: OwnedReadHalf,
}

impl TunnelServerInboundStream {
    pub fn new(
        read_half: OwnedReadHalf,
        crypt: Crypt,
        sender: Sender<Vec<u8>>,
        stop: Trigger,
    ) -> Self {
        TunnelServerInboundStream {
            stop,
            sender,
            crypt,
            buffer: PacketBuffer::new(),
            read_half,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        let mut header_buffer: [u8; HEADER_LENGTH] = [0; HEADER_LENGTH];

        loop {
            if Self::read_stream(
                &self.stop,
                &mut self.read_half,
                header_buffer.as_mut(),
                HEADER_LENGTH,
            )
            .await?
                == 0
            {
                // Connection closed
                break;
            }
            // Check valid header and get payload length
            let (counter, length) = parse_header(&header_buffer[..HEADER_LENGTH])?;
            // Read the encrypted payload + tag
            if Self::read_stream(
                &self.stop,
                &mut self.read_half,
                self.buffer.as_mut_slice(),
                length as usize,
            )
            .await?
                == 0
            {
                // Connection closed
                break;
            }
            let decrypted_data = self
                .crypt
                .decrypt(counter, length, &mut self.buffer)?
                .to_vec();
            self.sender.send(decrypted_data).map_err(|e| {
                anyhow::anyhow!("failed to send decrypted data to receiver: {:?}", e)
            })?;
        }
        self.stop.set();
        Ok(())
    }

    pub async fn read_stream(
        stop: &Trigger,
        read_half: &mut OwnedReadHalf,
        buffer: &mut [u8],
        length: usize,
    ) -> Result<usize> {
        tokio::select! {
            _ = stop.async_wait() => {
                log::info!("Inbound stream stopped while reading");
                Ok(0)  // Indicate end of processing
            }
            result = read_half.read_exact(&mut buffer[..length]) => {
                match result {
                    Ok(0) => {
                        // Connection closed
                        Ok(0)
                    }
                    Ok(n) => {
                        if n == length {
                            Ok(n)
                        } else {
                            // Incomplete read is an error in this context
                            Err(anyhow::anyhow!("incomplete read"))
                        }
                    }
                    Err(e) => {
                        Err(anyhow::format_err!("read error: {:?}", e))
                    }
                }
            }
        }
    }
}

struct TunnelServerOutboundStream {
    stop: Trigger,
    receiver: Receiver<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    write_half: OwnedWriteHalf,
}

impl TunnelServerOutboundStream {
    pub fn new(
        write_half: OwnedWriteHalf,
        crypt: Crypt,
        receiver: Receiver<Vec<u8>>,
        stop: Trigger,
    ) -> Self {
        TunnelServerOutboundStream {
            stop,
            receiver,
            crypt,
            buffer: PacketBuffer::new(),
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
                            self.send_data(&data).await?
                        }
                        Err(_) => {
                            return Err(anyhow::anyhow!("Receiver channel closed"));
                        }
                    }
                }
            }
        }
        self.stop.set();
        Ok(())
    }

    async fn send_packet(&mut self, chunk: &[u8]) -> Result<()> {
        // Copy data to buffer
        let buf = self.buffer.as_mut_slice();
        buf[..chunk.len()].copy_from_slice(chunk);

        // Crypt
        let encrypted = self.crypt.encrypt(chunk.len(), &mut self.buffer)?;

        // Header
        let counter = self.crypt.get_counter();
        let length = encrypted.len() as u16;

        let mut header = [0u8; HEADER_LENGTH];
        build_header(counter, length, &mut header)?;

        // Send
        self.write_half.write_all(&header).await?;
        self.write_half.write_all(encrypted).await?;

        Ok(())
    }

    async fn send_data(&mut self, data: &[u8]) -> Result<()> {
        let mut offset = 0;

        while offset < data.len() {
            let end = (offset + CRYPT_PACKET_SIZE).min(data.len());
            let chunk = &data[offset..end];

            self.send_packet(chunk).await?;
            offset = end;
        }

        Ok(())
    }
}

pub fn run_tunnel_stream(
    stream: TcpStream,
    inbound_crypt: Crypt,
    outbound_crypt: Crypt,
    stop: Trigger,
) {
    let (read_half, write_half) = stream.into_split();
    let (sender, receiver) = flume::unbounded();
    let local_stop = Trigger::new();

    let mut inbound =
        TunnelServerInboundStream::new(read_half, inbound_crypt, sender, local_stop.clone());

    let mut outbound =
        TunnelServerOutboundStream::new(write_half, outbound_crypt, receiver, local_stop.clone());

    tokio::spawn(async move {
        if let Err(e) = inbound.run().await {
            log::error!("Inbound stream error: {:?}", e);
        }
    });
    tokio::spawn(async move {
        if let Err(e) = outbound.run().await {
            log::error!("Outbound stream error: {:?}", e);
        }
    });

    // Global or local stop can stop both inbound and outbound streams
    tokio::spawn(async move {
        tokio::select! {
            _ = stop.async_wait() => {
                local_stop.set();
            }
            _ = local_stop.async_wait() => {
                // do nothing, just wait for local stop
            }
        }
    });
}
