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

use anyhow::{Result, anyhow, ensure};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::AsyncReadExt;

// https://github.com/haproxy/haproxy/blob/master/doc/proxy-protocol.txt

use super::consts::PROXY_V2_SIGNATURE;

#[derive(Debug)]
pub struct ProxyInfo {
    pub version: u8,
    pub command: u8,
    pub family: u8,
    pub protocol: u8,
    pub address_length: usize,

    pub source_addr: SocketAddr,
    pub dest_addr: SocketAddr,

    pub tlvs: Vec<u8>,
}

impl ProxyInfo {
    pub async fn read_from_stream<R>(stream: &mut R) -> Result<ProxyInfo>
    where
        R: AsyncReadExt + Unpin,
    {
        let mut header_buf = [0u8; 16];
        stream.read_exact(&mut header_buf).await?;

        ensure!(
            header_buf[..12] == PROXY_V2_SIGNATURE,
            "invalid PROXY v2 signature"
        );

        let ver_cmd = header_buf[12];
        // let fam_proto = header_buf[13];
        let len = u16::from_be_bytes([header_buf[14], header_buf[15]]) as usize;

        ensure!((ver_cmd >> 4) == 0x2, "not PROXY protocol v2");
        ensure!(
            (ver_cmd & 0x0F) == 0x1,
            "unsupported PROXY command (only PROXY=1 allowed)"
        );

        let total_len = 16 + len;
        let mut full_buf = vec![0u8; total_len];
        full_buf[..16].copy_from_slice(&header_buf);

        stream.read_exact(&mut full_buf[16..]).await?;

        ProxyInfo::parse(&full_buf)
    }

    /// Expect a full pre-checked PROXY v2 buffer
    /// As it only used on inner functions and tests
    /// we skip some checks already done on read_from_stream
    fn parse(buf: &[u8]) -> Result<ProxyInfo> {
        let ver_cmd = buf[12];
        let fam_proto = buf[13];
        let len = u16::from_be_bytes([buf[14], buf[15]]) as usize;

        let version = ver_cmd >> 4;
        let command = ver_cmd & 0x0F;

        let family = fam_proto >> 4;
        let protocol = fam_proto & 0x0F;

        // ensure!(
        //     buf.len() >= 16 + len,
        //     "buffer too small for declared PROXY v2 length"
        // );

        let addr_data = &buf[16..16 + len];

        let (source_addr, dest_addr, consumed) = match (family, protocol) {
            (0x1, 0x1) => ProxyInfo::parse_tcp4(addr_data)?,
            (0x2, 0x1) => ProxyInfo::parse_tcp6(addr_data)?,
            _ => return Err(anyhow!("unsupported address family/protocol")),
        };

        let tlvs = addr_data[consumed..].to_vec();

        Ok(ProxyInfo {
            version, // always 2
            command, // always 1 (PROXY)
            family,
            protocol,
            address_length: len,
            source_addr,
            dest_addr,
            tlvs,
        })
    }

    fn parse_tcp4(data: &[u8]) -> Result<(SocketAddr, SocketAddr, usize)> {
        ensure!(data.len() >= 12, "invalid TCP4 address length");

        let src_ip = Ipv4Addr::new(data[0], data[1], data[2], data[3]);
        let dst_ip = Ipv4Addr::new(data[4], data[5], data[6], data[7]);

        let src_port = u16::from_be_bytes([data[8], data[9]]);
        let dst_port = u16::from_be_bytes([data[10], data[11]]);

        Ok((
            SocketAddr::new(src_ip.into(), src_port),
            SocketAddr::new(dst_ip.into(), dst_port),
            12,
        ))
    }

    fn parse_tcp6(data: &[u8]) -> Result<(SocketAddr, SocketAddr, usize)> {
        ensure!(data.len() >= 36, "invalid TCP6 address length");

        let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[0..16])?);
        let dst_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&data[16..32])?);

        let src_port = u16::from_be_bytes([data[32], data[33]]);
        let dst_port = u16::from_be_bytes([data[34], data[35]]);

        Ok((
            SocketAddr::new(src_ip.into(), src_port),
            SocketAddr::new(dst_ip.into(), dst_port),
            36,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxy_v2_ipv4_no_tlvs() {
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

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let info = ProxyInfo::read_from_stream(&mut stream).await.unwrap();

        assert_eq!(info.source_addr.to_string(), "192.168.1.10:8080");
        assert_eq!(info.dest_addr.to_string(), "10.0.0.5:80");
        assert!(info.tlvs.is_empty());
    }

    #[tokio::test]
    async fn test_proxy_v2_ipv6_no_tlvs() {
        let mut buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
            0x21, // version=2, command=1
            0x21, // family=2 (IPv6), proto=1 (TCP)
            0x00, 0x24, // len = 36
        ];

        // IPv6 block: 16 src + 16 dst + 2 src port + 2 dst port
        buf.extend_from_slice(&[
            // src IP
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, // dst IP
            0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, // src port
            0x1F, 0x90, // dst port
            0x00, 0x50,
        ]);

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let info = ProxyInfo::read_from_stream(&mut stream).await.unwrap();

        assert_eq!(info.source_addr.to_string(), "[::1]:8080");
        assert_eq!(info.dest_addr.to_string(), "[2001:db8::1]:80");
        assert!(info.tlvs.is_empty());
    }

    #[tokio::test]
    async fn test_proxy_v2_ipv4_with_tlvs() {
        let buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x11,
            0x00, 0x0F, // len = 15 (12 IPv4 + 3 TLV)
            192, 168, 1, 10, 10, 0, 0, 5, 0x1F, 0x90, 0x00, 0x50, // TLV (3 bytes)
            0x01, 0x02, 0x03,
        ];

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let info = ProxyInfo::read_from_stream(&mut stream).await.unwrap();

        assert_eq!(info.tlvs, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn test_invalid_signature() {
        let buf = vec![0x00; 16]; // firma incorrecta

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let err = ProxyInfo::read_from_stream(&mut stream).await.unwrap_err();
        assert!(err.to_string().contains("invalid PROXY v2 signature"));
    }

    #[tokio::test]
    async fn test_invalid_version() {
        let buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
            0x11, // version=1 (incorrecto)
            0x11, 0x00, 0x0C,
        ];

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let err = ProxyInfo::read_from_stream(&mut stream).await.unwrap_err();
        assert!(err.to_string().contains("not PROXY protocol v2"));
    }

    #[tokio::test]
    async fn test_invalid_command() {
        let buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
            0x20, // version=2, command=0 (LOCAL)
            0x11, 0x00, 0x0C,
        ];

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let err = ProxyInfo::read_from_stream(&mut stream).await.unwrap_err();
        assert!(err.to_string().contains("unsupported PROXY command"));
    }

    #[tokio::test]
    async fn test_invalid_length() {
        let buf = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x11,
            0x00, 0x0C, // len=12
                  // but no address data provided
        ];

        let mut stream = tokio::io::BufReader::new(std::io::Cursor::new(buf));

        let err = ProxyInfo::read_from_stream(&mut stream).await.unwrap_err();
        println!("Error: {}", err);
        assert!(err.to_string().contains("early eof"));
    }
}
