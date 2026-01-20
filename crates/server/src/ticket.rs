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
use anyhow::Result;
use rand::{Rng, distr::Alphanumeric};

use crate::consts::TICKET_LENGTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket([u8; TICKET_LENGTH]);

impl Ticket {
    pub fn new() -> Self {
        let rng = rand::rng();
        let id = rng
            .sample_iter(Alphanumeric)
            .take(TICKET_LENGTH)
            .collect::<Vec<u8>>()
            .try_into()
            .expect("Failed to create Ticket");
        Ticket(id)
    }
    pub fn from(id: [u8; TICKET_LENGTH]) -> Self {
        Ticket(id)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.0.iter().all(|&c| c.is_ascii_alphanumeric()) {
            return Err(anyhow::anyhow!("Invalid ticket"));
        }
        Ok(())
    }
}

impl Default for Ticket {
    fn default() -> Self {
        Ticket::new()
    }
}

impl From<[u8; TICKET_LENGTH]> for Ticket {
    fn from(id: [u8; TICKET_LENGTH]) -> Self {
        Ticket::from(id)
    }
}

impl From<&[u8; TICKET_LENGTH]> for Ticket {
    fn from(id: &[u8; TICKET_LENGTH]) -> Self {
        Ticket::from(*id)
    }
}
