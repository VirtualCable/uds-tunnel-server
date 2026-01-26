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

// Authors: Adolfo Gómez, dkmaster at dkmon dot compub mod broker;

#![allow(dead_code, unused_variables)]
use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::Duration,
    time::timeout,
};

use shared::{
    errors::ErrorWithAddres, log, protocol::consts::HANDSHAKE_TIMEOUT_MS, protocol::handshake,
};

mod connect;
mod recover;
mod types;

pub async fn handle_connection<R, W>(
    mut reader: R,
    writer: W,
    connection_ip: std::net::SocketAddr,
    use_proxy_v2: bool,
) -> Result<(), ErrorWithAddres>
where
    R: AsyncReadExt + Unpin + Send + 'static,
    W: AsyncWriteExt + Unpin + Send + 'static,
{
    log::debug!("Starting connection handshake");
    let (ip, action) = match timeout(
        Duration::from_millis(HANDSHAKE_TIMEOUT_MS),
        handshake::Handshake::parse(&mut reader, use_proxy_v2),
    )
    .await
    .map_err(|_| ErrorWithAddres::new(Some(connection_ip), "Handshake timed out"))?
    {
        // If no ip is provided from handshake, use connection ip
        Ok(h) => {
            let ip = h.src_ip.unwrap_or(connection_ip);
            (ip, h.action)
        }
        Err(e) => {
            let ip = e.src_ip.unwrap_or(connection_ip);
            // Handshake failed
            return Err(ErrorWithAddres::new(
                Some(ip),
                format!("Handshake failed: {}", e).as_str(),
            ));
        }
    };

    match action {
        handshake::HandshakeAction::Test => {
            // Just close the connection
            Ok(())
        }
        handshake::HandshakeAction::Open { ticket } => {
            connect::connect(reader, writer, &ticket, ip)
                .await
                .map_err(|e| {
                    ErrorWithAddres::new(Some(ip), format!("Connection failed: {:?}", e).as_str())
                })
        }
        handshake::HandshakeAction::Recover { ticket } => {
            // TODO: implement
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
