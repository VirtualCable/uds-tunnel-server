use std::net::SocketAddr;

use anyhow::{Result, ensure};
use num_enum::{FromPrimitive, IntoPrimitive};
use tokio::io::AsyncReadExt;

use crate::{
    consts::{HANDSHAKE_V2_SIGNATURE, TICKET_LENGTH},
    proxy_v2_protocol::ProxyInfo,
};

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
//        Open | ticket[48] | ticket encrpyted with HKDF-derived key
//        Recover | ticket[48] | ticket encrypted with HKDF-derived key
//   - Full handshake should occur on at most 0.2 seconds
//   - Any failed handhsake, closes without response (hide server presence as much as possible)
//   - TODO: Make some kind of block by IP if too many failed handshakes in short time

pub enum HandshakeAction {
    Test,
    Open { ticket: [u8; TICKET_LENGTH] },
    Recover { ticket: [u8; TICKET_LENGTH] },
}

pub struct Handshake {
    pub src_ip: Option<SocketAddr>,
    pub action: HandshakeAction,
}

pub async fn parse_handshake<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    expect_proxy_v2: bool,
) -> Result<Handshake> {
    let ip = if expect_proxy_v2 {
        let proxy_info = ProxyInfo::read_from_stream(reader).await?;
        Some(proxy_info.source_addr)
    } else {
        None
    };
    let mut signature_buf = [0u8; HANDSHAKE_V2_SIGNATURE.len() + 1];
    reader.read_exact(&mut signature_buf).await?;
    ensure!(
        signature_buf[..HANDSHAKE_V2_SIGNATURE.len()] == *HANDSHAKE_V2_SIGNATURE,
        "invalid handshake v2 signature"
    );
    let cmd: HandshakeCommand = signature_buf[HANDSHAKE_V2_SIGNATURE.len()].into();
    match cmd {
        HandshakeCommand::Test => Ok(Handshake {
            src_ip: ip,
            action: HandshakeAction::Test,
        }),
        HandshakeCommand::Open | HandshakeCommand::Recover => {
            let mut ticket_buf = [0u8; TICKET_LENGTH];
            reader.read_exact(&mut ticket_buf).await?;
            let action = match cmd {
                HandshakeCommand::Open => HandshakeAction::Open { ticket: ticket_buf },
                HandshakeCommand::Recover => HandshakeAction::Recover { ticket: ticket_buf },
                _ => unreachable!(),
            };
            Ok(Handshake { src_ip: ip, action })
        }
        HandshakeCommand::Unknown => Err(anyhow::anyhow!("unknown handshake command")),
    }
}
