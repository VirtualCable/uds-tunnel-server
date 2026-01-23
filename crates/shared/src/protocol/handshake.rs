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

use std::net::SocketAddr;

use anyhow::Result;
use num_enum::{FromPrimitive, IntoPrimitive};
use tokio::io::AsyncReadExt;

use crate::{consts::TICKET_LENGTH, errors::ErrorWithAddres, log, ticket::Ticket};

use super::proxy_v2::ProxyInfo;

use super::consts::HANDSHAKE_V2_SIGNATURE;

// Handshake commands, starting from 0
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum HandshakeCommand {
    Test = 0,
    Open = 1,
    Recover = 2,
    #[num_enum(default)]
    Unknown = 255,
}

// Posible handshakes:
//   - With or without PROXY protocol v2 header
//   - HANDSHAKE_V2 | cmd:u8 | payload_cmd_dependent
//        Test | no payload
//        Open | ticket[48] | ticket encrpyted with HKDF-derived key  --> returns session id for new session
//        Recover | ticket[48] | ticket encrypted with HKDF-derived key (this ticket is the session id of the lost session) -> returns same as Open (new session id)
//   - Full handshake should occur on at most 0.2 seconds
//   - Any failed handhsake, closes without response (hide server presence as much as possible)
//   - TODO: Make some kind of block by IP if too many failed handshakes in short time

pub enum HandshakeAction {
    Test,
    Open { ticket: Ticket },
    Recover { ticket: Ticket },
}

pub struct Handshake {
    pub src_ip: Option<SocketAddr>,
    pub action: HandshakeAction,
}

impl Handshake {
    pub async fn parse<R: AsyncReadExt + Unpin>(
        reader: &mut R,
        use_proxy_v2: bool,
    ) -> Result<Handshake, ErrorWithAddres> {
        let ip = if use_proxy_v2 {
            let proxy_info = ProxyInfo::read_from_stream(reader).await.map_err(|e| {
                ErrorWithAddres::new(
                    None,
                    format!("failed to read PROXY protocol v2 header: {}", e).as_str(),
                )
            })?;
            log::debug!("Received PROXY v2 info: {:?}", proxy_info);
            Some(proxy_info.source_addr)
        } else {
            None
        };
        let mut signature_buf = [0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
        reader
            .read_exact(&mut signature_buf)
            .await
            .map_err(|e| ErrorWithAddres {
                src_ip: ip,
                message: format!("failed to read handshake signature and command: {}", e),
            })?;
        if signature_buf.len() != HANDSHAKE_V2_SIGNATURE.len() + 1 {
            return Err(ErrorWithAddres {
                src_ip: ip,
                message: "incomplete handshake signature and command".to_string(),
            });
        }
        let cmd: HandshakeCommand = signature_buf[HANDSHAKE_V2_SIGNATURE.len()].into();
        match cmd {
            HandshakeCommand::Test => Ok(Handshake {
                src_ip: ip,
                action: HandshakeAction::Test,
            }),
            HandshakeCommand::Open | HandshakeCommand::Recover => {
                let mut ticket_buf = [0u8; TICKET_LENGTH];
                reader.read_exact(&mut ticket_buf).await.map_err(|e| {
                    ErrorWithAddres::new(
                        ip,
                        format!("failed to read handshake ticket: {}", e).as_str(),
                    )
                })?;
                let action = match cmd {
                    HandshakeCommand::Open => HandshakeAction::Open {
                        ticket: ticket_buf.into(),
                    },
                    HandshakeCommand::Recover => HandshakeAction::Recover {
                        ticket: ticket_buf.into(),
                    },
                    _ => unreachable!(),
                };
                Ok(Handshake { src_ip: ip, action })
            }
            HandshakeCommand::Unknown => Err(ErrorWithAddres::new(ip, "unknown handshake command")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handshake_parse_no_proxy_test() {
        let mut data = Vec::new();
        data.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
        data.push(HandshakeCommand::Test.into());
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let handshake = Handshake::parse(&mut reader, false).await.unwrap();
        assert!(handshake.src_ip.is_none());
        match handshake.action {
            HandshakeAction::Test => {}
            _ => panic!("expected Test action"),
        }
    }

    #[tokio::test]
    async fn test_handshake_parse_no_proxy_open() {
        let mut data = Vec::new();
        data.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
        data.push(HandshakeCommand::Open.into());
        let ticket = [0x42u8; TICKET_LENGTH];
        data.extend_from_slice(&ticket);
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let handshake = Handshake::parse(&mut reader, false).await.unwrap();
        assert!(handshake.src_ip.is_none());
        match handshake.action {
            HandshakeAction::Open { ticket: t } => {
                assert_eq!(t, ticket.into());
            }
            _ => panic!("expected Open action"),
        }
    }

    #[tokio::test]
    async fn test_handshake_parse_no_proxy_recover() {
        let mut data = Vec::new();
        data.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
        data.push(HandshakeCommand::Recover.into());
        let ticket = [0x43u8; TICKET_LENGTH];
        data.extend_from_slice(&ticket);
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let handshake = Handshake::parse(&mut reader, false).await.unwrap();
        assert!(handshake.src_ip.is_none());
        match handshake.action {
            HandshakeAction::Recover { ticket: t } => {
                assert_eq!(t, ticket.into());
            }
            _ => panic!("expected Recover action"),
        }
    }

    #[tokio::test]
    async fn test_handshake_parse_invalid_signature() {
        let mut data = Vec::new();
        data.extend_from_slice(b"INVALID_SIGNATURE");
        data.push(HandshakeCommand::Test.into());
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let result = Handshake::parse(&mut reader, false).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_handshake_parse_proxy_test() {
        let mut data = Vec::new();
        // https://github.com/haproxy/haproxy/blob/master/doc/proxy-protocol.txt
        // PROXY v2 header:
        // signature (12 bytes)
        // ver_cmd = 0x21 (version 2, command PROXY)
        // fam_proto = 0x11 (INET + STREAM)
        // len = 12 (IPv4 block)
        let buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
            0x21, // version=2, command=1
            0x11, // family=1 (IPv4), proto=1 (TCP)
            0x00, 0x0C, // len = 12
            // IPv4 block:
            192, 168, 1, 10, // src IP
            10, 0, 0, 5, // dst IP
            0x1F, 0x90, // src port 8080
            0x00, 0x50, // dst port 80
        ];

        data.extend_from_slice(&buf);
        data.extend_from_slice(HANDSHAKE_V2_SIGNATURE);
        data.push(HandshakeCommand::Test.into());
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let handshake = Handshake::parse(&mut reader, true).await.unwrap();
        assert!(handshake.src_ip.is_some());
        match handshake.action {
            HandshakeAction::Test => {}
            _ => panic!("expected Test action"),
        }
    }
}
