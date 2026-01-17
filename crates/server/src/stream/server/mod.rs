use anyhow::Result;
use flume::{Receiver, Sender};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::{
    consts::SERVER_RECOVERY_GRACE_SECS, // global crate consts
    crypt::{
        Crypt, build_header,
        consts::{CRYPT_PACKET_SIZE, HEADER_LENGTH},
        parse_header,
        types::PacketBuffer,
    },
    log,
    session::{SessionId, get_session_manager},
    system::trigger::Trigger,
};

struct TunnelServerInboundStream<R: AsyncReadExt + Unpin> {
    stop: Trigger,
    sender: Sender<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    read_half: R,
}

impl<R: AsyncReadExt + Unpin> TunnelServerInboundStream<R> {
    pub fn new(read_half: R, crypt: Crypt, sender: Sender<Vec<u8>>, stop: Trigger) -> Self {
        TunnelServerInboundStream {
            stop,
            sender,
            crypt,
            buffer: PacketBuffer::new(),
            read_half,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting server inbound stream");
        let mut header_buffer: [u8; HEADER_LENGTH] = [0; HEADER_LENGTH];

        loop {
            if Self::read_stream(
                &self.stop,
                &mut self.read_half,
                header_buffer.as_mut(),
                HEADER_LENGTH,
                false,
            )
            .await?
                == 0
            {
                // Connection closed
                log::info!("Inbound stream closed while reading header");
                break;
            }
            // Check valid header and get payload length
            let (seq, length) = parse_header(&header_buffer[..HEADER_LENGTH])?;
            // Read the encrypted payload + tag
            if Self::read_stream(
                &self.stop,
                &mut self.read_half,
                self.buffer.as_mut_slice(),
                length as usize,
                true,
            )
            .await?
                == 0
            {
                // Connection closed
                log::info!("Inbound stream closed while reading payload");
                break;
            }
            let decrypted_data = self
                .crypt
                .decrypt(seq, length, &mut self.buffer)?
                .to_vec();
            self.sender.try_send(decrypted_data)?;
        }
        Ok(())
    }

    pub async fn read_stream(
        stop: &Trigger,
        read_half: &mut R,
        buffer: &mut [u8],
        length: usize,
        disallow_eof: bool,
    ) -> Result<usize> {
        let mut read = 0;

        while read < length {
            let n = tokio::select! {
                _ = stop.async_wait() => {
                    log::info!("Inbound stream stopped while reading");
                    return Ok(0);  // Indicate end of processing
                }
                result = read_half.read(&mut buffer[read..length]) => {
                    match result {
                        Ok(0) => {
                            if disallow_eof || read != 0 {
                                return Err(anyhow::anyhow!("connection closed unexpectedly"));
                            } else {
                                return Ok(0);  // Connection closed
                            }
                        }
                        Ok(n) => n,
                        Err(e) => {
                            return Err(anyhow::format_err!("read error: {:?}", e));
                        }
                    }
                }
            };
            read += n;
        }
        Ok(read)
    }
}

struct TunnelServerOutboundStream<W: AsyncWriteExt + Unpin> {
    stop: Trigger,
    receiver: Receiver<Vec<u8>>,
    buffer: PacketBuffer,
    crypt: Crypt,

    write_half: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelServerOutboundStream<W> {
    pub fn new(write_half: W, crypt: Crypt, receiver: Receiver<Vec<u8>>, stop: Trigger) -> Self {
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
        Ok(())
    }

    async fn send_packet(&mut self, chunk: &[u8]) -> Result<()> {
        // Copy data to buffer
        let buf = self.buffer.as_mut_slice();
        buf[..chunk.len()].copy_from_slice(chunk);

        // Crypt
        let encrypted = self.crypt.encrypt(chunk.len(), &mut self.buffer)?;

        // Header
        let counter = self.crypt.current_seq();
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

/// Runs a tunnel stream with inbound and outbound processing
/// # Arguments
/// * `stream` - The TCP stream to handle
/// * `inbound_crypt` - Crypt object for inbound data decryption
/// * `inbound_channel` - Receiver channel for inbound data (from Server side)
/// * `outbound_crypt` - Crypt object for outbound data encryption
/// * `outbound_channel` - Sender channel for outbound data (to Server side)
/// * `stop` - Trigger to stop the stream
/// # Returns
/// Nothing, runs indefinitely until stopped
///
/// Note: "Server side" is the side that communicates with the remote Server
pub struct TunnelServerStream {
    session_id: SessionId,
    stream: TcpStream,
    inbound_crypt: Crypt,
    outbound_crypt: Crypt,
}

impl TunnelServerStream {
    pub fn new(
        session_id: SessionId,
        stream: TcpStream,
        inbound_crypt: Crypt,
        outbound_crypt: Crypt,
    ) -> Self {
        Self {
            session_id,
            stream,
            inbound_crypt,
            outbound_crypt,
        }
    }

    pub async fn run(self) -> Result<()> {
        let Self {
            session_id,
            stream,
            inbound_crypt,
            outbound_crypt,
        } = self;

        let (read_half, write_half) = stream.into_split();
        let (stop, channels) = if let Some(session) = get_session_manager().get_session(&session_id)
        {
            (
                session.get_stop_trigger(),
                session.get_client_channels().await?,
            )
        } else {
            log::warn!("Session {:?} not found, aborting stream", session_id);
            return Ok(());
        };

        let local_stop = Trigger::new();

        let mut inbound = TunnelServerInboundStream::new(
            read_half,
            inbound_crypt,
            channels.0,
            local_stop.clone(),
        );

        let mut outbound = TunnelServerOutboundStream::new(
            write_half,
            outbound_crypt,
            channels.1,
            local_stop.clone(),
        );

        tokio::spawn(async move {
            if let Err(e) = inbound.run().await {
                log::error!("Inbound stream error: {:?}", e);
            }
            // let's ensure the other side is also stopped
            inbound.stop.set();
        });

        tokio::spawn(async move {
            if let Err(e) = outbound.run().await {
                log::error!("Outbound stream error: {:?}", e);
            }
            // let's ensure the other side is also stopped
            outbound.stop.set();
        });

        tokio::spawn(async move {
            // Notify starting server side
            if let Err(e) = get_session_manager().start_server(&session_id).await {
                log::error!("Failed to start server session {:?}: {:?}", session_id, e);
                local_stop.set();
                // Note: Server side does not trigger stop of the session on failure
                //       as it is recoverable.
                return;
            }
            tokio::select! {
                _ = stop.async_wait() => {
                    local_stop.set();
                }
                _ = local_stop.async_wait() => {}
            }
            // Notify stopping server side
            if let Err(e) = get_session_manager().stop_server(&session_id).await {
                log::error!("Failed to stop server session {:?}: {:?}", session_id, e);
            }

            // Insert a task that, after a couple of seconds, sets the stop trigger
            // if the session already exists and the server is not running
            tokio::spawn({
                let stop = stop.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(SERVER_RECOVERY_GRACE_SECS))
                        .await;
                    if let Some(session) = get_session_manager().get_session(&session_id)
                        && !session.is_server_running()
                    {
                        log::info!(
                            "Server side not running for session {:?}, setting stop trigger",
                            session_id
                        );
                        stop.set();
                    }
                }
            });
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;