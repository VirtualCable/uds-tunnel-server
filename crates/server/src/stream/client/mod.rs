use anyhow::Result;
use flume::{Receiver, Sender};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
};

use crate::{
    crypt::consts::CRYPT_PACKET_SIZE,
    log,
    session::{SessionId, get_session_manager},
    system::trigger::Trigger,
};

pub struct TunnelClientInboundStream<R: AsyncReadExt + Unpin> {
    stop: Trigger,
    sender: Sender<Vec<u8>>,

    reader: R,
}

impl<R: AsyncReadExt + Unpin> TunnelClientInboundStream<R> {
    pub fn new(reader: R, sender: Sender<Vec<u8>>, stop: Trigger) -> Self {
        TunnelClientInboundStream {
            stop,
            sender,
            reader,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        log::debug!("Starting client inbound stream");
        // Read from read_half, raw, decrypt and send to sender channel, raw
        let mut buffer = [0u8; CRYPT_PACKET_SIZE];
        loop {
            tokio::select! {
                _ = self.stop.async_wait() => {
                    break;
                }
                result = self.reader.read(&mut buffer) => {
                    match result {
                        Ok(0) => {
                            // Connection closed
                            break;
                        }
                        Ok(count) => {
                            // Send to channel, fail if full or disconnected
                            // Does not wait for space in channel
                            self.sender.try_send(buffer[..count].to_vec())?;
                        }
                        Err(e) => {
                            log::error!("Client inbound read error: {:?}", e);
                            // Set stop and return error
                            self.stop.set();
                            return Err(anyhow::anyhow!("Client inbound read error: {:?}", e));
                        }
                    }
                }
            }
        }
        // Ensure stop is set
        self.stop.set();
        Ok(())
    }
}

struct TunnelClientOutboundStream<W: AsyncWriteExt + Unpin> {
    stop: Trigger,
    receiver: Receiver<Vec<u8>>,

    writer: W,
}

impl<W: AsyncWriteExt + Unpin> TunnelClientOutboundStream<W> {
    pub fn new(writer: W, receiver: Receiver<Vec<u8>>, stop: Trigger) -> Self {
        TunnelClientOutboundStream {
            stop,
            receiver,
            writer,
        }
    }
    pub async fn run(&mut self) -> Result<()> {
        // Run on client side is mandatory. If run ends, stop must be set. in any case.
        log::debug!("Starting client outbound stream");
        loop {
            tokio::select! {
                _ = self.stop.async_wait() => {
                    break;
                }
                result = self.receiver.recv_async() => {
                    match result {
                        Ok(data) => {
                            self.writer.write_all(&data).await?;
                        }
                        Err(_) => {
                            log::error!("Client outbound receiver channel closed");
                            self.stop.set();
                            return Err(anyhow::anyhow!("Receiver channel closed"));
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

pub struct TunnelClientStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    session_id: SessionId,
    reader: R,
    writer: W
}

impl<R, W> TunnelClientStream<R, W>
where
    R: AsyncReadExt + Send + Unpin + 'static,
    W: AsyncWriteExt + Send + Unpin + 'static,
{
    pub fn new(id: SessionId, reader: R, writer: W) -> Self {
        TunnelClientStream {
            session_id: id,
            reader,
            writer,
        }
    }

    pub async fn run(self) -> Result<()> {
        let Self { session_id, reader, writer } = self;

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

        let mut inbound = TunnelClientInboundStream::new(reader, channels.0, local_stop.clone());

        let mut outbound =
            TunnelClientOutboundStream::new(writer, channels.1, local_stop.clone());
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
            if let Err(e) = get_session_manager().start_client(&session_id).await {
                log::error!("Failed to start client session {:?}: {:?}", session_id, e);
                local_stop.set();
                stop.set();
                return;
            }
            tokio::select! {
                _ = stop.async_wait() => {
                    local_stop.set();
                }
                _ = local_stop.async_wait() => {
                    stop.set();
                }
            }
            // Notify stopping client side
            if let Err(e) = get_session_manager().stop_client(&session_id).await {
                log::error!("Failed to stop client session {:?}: {:?}", session_id, e);
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;