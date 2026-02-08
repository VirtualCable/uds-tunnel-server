// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot com
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::{
    io::AsyncWriteExt,
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
};

use shared::{log, system::trigger::Trigger};

use super::{
    crypt::{tunnel::get_tunnel_crypts, types::PacketBuffer},
    protocol::{handshake::Handshake, ticket::Ticket},
    server::TunnelServer,
};

mod handler;
mod servers;

pub use handler::Handler;

pub struct Proxy {
    tunnel_server: String, // Host:port of tunnel server to connect to
    ticket: Ticket,
    shared_secret: [u8; 32],
    stop: Trigger,
    initial_timeout: std::time::Duration,

    recover_connection: bool,
}

impl Proxy {
    pub fn new(
        tunnel_server: &str,
        ticket: Ticket,
        shared_secret: [u8; 32],
        initial_timeout: Duration,
    ) -> Self {
        Self {
            tunnel_server: tunnel_server.to_string(),
            ticket,
            shared_secret,
            stop: Trigger::new(),
            initial_timeout,
            recover_connection: false,
        }
    }

    async fn connect(&mut self) -> Result<(OwnedReadHalf, OwnedWriteHalf)> {
        // Try to connect to tunnel server and authenticate using the ticket and shared secret
        let stream = tokio::net::TcpStream::connect(&self.tunnel_server)
            .await
            .context("Failed to connect to tunnel server")?;

        // Try to disable Nagle's algorithm for better performance in our case
        stream.set_nodelay(true).ok();

        // Create the crypt pair
        let (mut inbound_crypt, mut outbound_crypt) =
            get_tunnel_crypts(&self.shared_secret.into(), &self.ticket, (0, 0))?;

        // Send open tunnel command with the ticket and shared secret
        let handshake = if self.recover_connection {
            Handshake::Recover {
                ticket: self.ticket,
            }
        } else {
            self.recover_connection = true; // Next time we will try to recover the connection
            Handshake::Open {
                ticket: self.ticket,
            }
        };
        // Split the stream into reader and writer for easier handling on the next steps
        let (mut reader, mut writer) = stream.into_split();

        handshake.write(&mut writer).await?;

        // Send the encrypted ticket now to channel 0
        outbound_crypt
            .write(&mut writer, 0, self.ticket.as_ref())
            .await?;

        // Read the response, should be the "reconnect" ticket, just in case some connection error
        let mut buffer = PacketBuffer::new();
        let (reconnect_ticket, channel_id) = inbound_crypt
            .read(&self.stop, &mut reader, &mut buffer)
            .await?;

        // Channel id should be 0 for handshake response, if not, something went wrong
        if channel_id != 0 {
            return Err(anyhow::anyhow!(
                "Expected handshake response on channel 0, got channel {}",
                channel_id
            ));
        }

        // Store reconnect ticket for future use.
        // This is different from original, and different for every conection
        self.ticket = Ticket::new(
            reconnect_ticket
                .try_into()
                .context("Invalid ticket format in handshake response")?,
        );

        Ok((reader, writer))
    }

    // Launchs (or relaunchs) the tunnel server, returns a handler to send commands to the server
    async fn launch_server(&mut self, ctrl_tx: flume::Sender<handler::Command>) -> Result<()> {
        // TODO: Retry 3 times with some small delay (total must not exceed 2 seconds, that is the grace time for the tunnel server)
        let (reader, writer) = self.connect().await?;

        // Create the server and run it in a separate task
        let server = TunnelServer::new(
            reader,
            writer,
            self.stop.clone(),
            handler::Handler::new(ctrl_tx.clone()),
        );
        tokio::spawn(async move {
            if let Err(e) = server.run().await {
                log::warn!("Tunnel server error: {:?}", e);
                ctrl_tx
                    .send_async(handler::Command::ServerError {
                        message: format!("{:?}", e),
                    })
                    .await
                    .ok();
            } else {
                ctrl_tx.send_async(handler::Command::ServerClose).await.ok();
            }
        });
        Ok(())
    }

    pub async fn run(mut self) -> Result<Handler> {
        let (ctrl_tx, ctrl_rx) = flume::bounded(8); // Not too much buffering, we want backpressure on commands

        // Launch server or return an error
        self.launch_server(ctrl_tx.clone()).await?;

        // Execute the proxy task
        tokio::spawn(async move {
            // Main loop to handle tunnel communication, moves self into the async task
            let mut buffer = PacketBuffer::new();
            loop {
                tokio::select! {
                    // Check for stop signal
                    _ = self.stop.wait_async() => {
                        break;
                    }

                    // Handle control commands from the TunnelHandler
                    cmd = ctrl_rx.recv_async() => {
                        match cmd {
                            Ok(cmd) => {
                                if let Err(e) = self.handle_command(cmd).await {
                                    eprintln!("Error handling command: {:?}", e);
                                    break;
                                }
                            }
                            Err(_) => {
                                // Control channel closed, we should stop
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(handler::Handler::new(ctrl_tx))
    }

    async fn handle_command(&self, cmd: handler::Command) -> Result<()>
    {
        // TODO: implement command handling, for now just log the command
        Ok(())
    }
}

// Tests module
#[cfg(test)]
mod tests;
