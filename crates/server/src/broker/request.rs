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
use std::net::SocketAddr;

use shared::ticket::Ticket;

#[derive(serde::Serialize)]
pub(super) struct TicketRequest {
    token: String,
    ticket: String,
    command: String,
    ip: String,
    sent: Option<u64>,
    recv: Option<u64>,
    kem_kyber_key: Option<String>, // Only used on start command
}

impl TicketRequest {
    pub fn new_start(
        ticket: &Ticket,
        ip: &SocketAddr,
        auth_token: &str,
        kem_kyber_key: String,
    ) -> Self {
        TicketRequest {
            token: auth_token.to_string(),
            ticket: ticket.as_str().to_string(),
            command: "start".to_string(),
            ip: ip.ip().to_string(),
            sent: None,
            recv: None,
            kem_kyber_key: Some(kem_kyber_key),
        }
    }

    pub fn new_stop(ticket: &Ticket, auth_token: &str, sent: u64, recv: u64) -> Self {
        TicketRequest {
            token: auth_token.to_string(),
            ticket: ticket.as_str().to_string(),
            command: "stop".to_string(),
            ip: "".to_string(),
            sent: Some(sent),
            recv: Some(recv),
            kem_kyber_key: None,
        }
    }
}
