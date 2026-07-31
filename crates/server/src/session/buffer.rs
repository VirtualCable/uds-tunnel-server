#![allow(dead_code)]
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
use anyhow::Result;

use std::collections::VecDeque;

use shared::{log, protocol::PayloadWithChannel};

pub struct RecoverySendBuffer {
    items: VecDeque<BufferedPacket>,

    // Max size in bytes
    max_bytes: usize,

    current_bytes: usize,
}

#[derive(Debug)]
pub struct BufferedPacket {
    pub seq: u64, // Sequence number of the packet
    pub data: PayloadWithChannel,
}

#[derive(thiserror::Error, Debug)]
pub enum RecoveryError {
    #[error("Cannot recover: requested sequence {requested} not found in recovery buffer")]
    NotFound { requested: u64 },
}

impl BufferedPacket {
    pub fn new(seq: u64, data: PayloadWithChannel) -> Self {
        Self { seq, data }
    }
}

impl RecoverySendBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            items: VecDeque::new(),
            max_bytes,
            current_bytes: 0,
        }
    }

    pub fn push(&mut self, seq: u64, data: PayloadWithChannel) -> Result<&PayloadWithChannel> {
        let item_size = data.len();
        if item_size > self.max_bytes {
            return Err(anyhow::anyhow!("Item size exceeds buffer capacity"));
        }

        // Evict old items if necessary
        while self.current_bytes + item_size > self.max_bytes {
            if let Some(old_item) = self.items.pop_front() {
                self.current_bytes -= old_item.data.len();
            } else {
                break; // No more items to evict
            }
        }

        // Add new item
        self.items.push_back(BufferedPacket::new(seq, data));
        self.current_bytes += item_size;
        // Fatal error if cannot recover the just added item :S
        Ok(&self.items.back().unwrap().data)
    }

    pub fn skip(&mut self, seq: u64) -> Result<(), RecoveryError> {
        // Defensive guard: `u64::MAX` is the sentinel produced by an underflow
        // in the caller (e.g. `in_seqs.0 - 1` when `in_seqs.0 == 0`). Reject it
        // up front so we don't walk the entire buffer for nothing.
        if seq == u64::MAX {
            return Err(RecoveryError::NotFound { requested: seq });
        }
        // skip items until we find the one with the requested sequence or we exhaust the buffer
        // If we exhaust the buffer without finding the requested sequence, return an error
        while let Some(item) = self.items.pop_front() {
            self.current_bytes -= item.data.len();
            if item.seq == seq {
                log::debug!(
                    "Found requested sequence {}, len {:?}",
                    seq,
                    item.data.len()
                );
                return Ok(()); // Found the requested sequence, stop skipping
            }
        }
        Err(RecoveryError::NotFound { requested: seq })
    }

    /// Smallest sequence number currently in the buffer, or 0 if empty.
    pub fn head_seq(&self) -> u64 {
        self.items.front().map(|item| item.seq).unwrap_or(0)
    }

    /// Largest sequence number currently in the buffer, or 0 if empty.
    pub fn tail_seq(&self) -> u64 {
        self.items.back().map(|item| item.seq).unwrap_or(0)
    }

    pub fn take_unsent_packet(&mut self) -> Option<(PayloadWithChannel, u64)> {
        self.items.pop_front().map(|item| {
            self.current_bytes -= item.data.len();
            (item.data, item.seq)
        })
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

impl std::fmt::Debug for RecoverySendBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoverySendBuffer")
            .field("items", &self.items.len())
            .field(
                "first_seq",
                &self.items.front().map(|item| item.seq).unwrap_or(0),
            )
            .field(
                "last_seq",
                &self.items.back().map(|item| item.seq).unwrap_or(0),
            )
            .field("max_bytes", &self.max_bytes)
            .field("current_bytes", &self.current_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper that builds a payload of the given size.
    fn make_payload(size: usize) -> PayloadWithChannel {
        // Subtract 2 bytes for the channel_id, which is part of the payload in this context
        PayloadWithChannel {
            channel_id: 0,
            payload: vec![0u8; size - 2].into(),
        }
    }

    #[test]
    fn push_and_len_is_correct() {
        let mut buf = RecoverySendBuffer::new(100);
        let returned = buf.push(10, make_payload(5)).unwrap();
        assert_eq!(returned.len(), 5);
        assert_eq!(buf.len(), 1);
        assert!(!buf.is_empty());
    }

    #[test]
    fn push_too_large_item_errors() {
        let mut buf = RecoverySendBuffer::new(5);
        let err = buf.push(1, make_payload(6)).unwrap_err();
        assert!(
            err.to_string()
                .contains("Item size exceeds buffer capacity")
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn eviction_happens_when_capacity_exceeded() {
        let mut buf = RecoverySendBuffer::new(10);
        buf.push(1, make_payload(3)).unwrap(); // total 3 + 2
        buf.push(2, make_payload(4)).unwrap(); // total 7
        buf.push(3, make_payload(5)).unwrap(); // total 12 → evict seq1
        assert_eq!(buf.len(), 2);

        // the remaining packets should be those with lengths 4 and 5
        let (p, seq) = buf.take_unsent_packet().unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(seq, 2);
        let (p, seq) = buf.take_unsent_packet().unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(seq, 3);
        assert!(buf.is_empty());
    }

    #[test]
    fn skip_finds_sequence_and_removes_up_to_it() {
        let mut buf = RecoverySendBuffer::new(100);
        buf.push(1, make_payload(3)).unwrap();
        buf.push(2, make_payload(4)).unwrap();
        buf.push(3, make_payload(5)).unwrap();

        buf.skip(2).expect("sequence 2 should be present");
        // only packet 3 should remain
        let (p, seq) = buf.take_unsent_packet().unwrap();
        assert_eq!(p.len(), 5);
        assert_eq!(seq, 3);
        assert!(buf.is_empty());
    }

    #[test]
    fn skip_not_found_returns_error() {
        let mut buf = RecoverySendBuffer::new(100);
        buf.push(1, make_payload(3)).unwrap();
        let err = buf.skip(99).unwrap_err();

        let RecoveryError::NotFound { requested } = err;
        assert_eq!(requested, 99);
    }

    #[test]
    fn take_unsent_packet_yields_insertion_order() {
        let mut buf = RecoverySendBuffer::new(100);
        for i in 1..=3 {
            buf.push(i, make_payload(i as usize + 2)).unwrap();
        }

        for expected in 1..=3 {
            let (p, seq) = buf.take_unsent_packet().unwrap();
            assert_eq!(p.len(), expected as usize + 2);
            assert_eq!(seq, expected);
        }

        assert!(buf.take_unsent_packet().is_none());
        assert!(buf.is_empty());
    }

    // The recover path used to call `buffer.skip(in_seqs.0 - 1)`
    // without validating the offset, which let a malformed Recover
    // handshake empty the buffer and tear down the recovered session.
    // The skip path now rejects `u64::MAX` upfront, and the caller
    // (connection/recover.rs) bounds-checks the requested sequence
    // against the buffer window before mutating anything.

    #[test]
    fn skip_rejects_u64_max_without_walking_buffer() {
        let mut buf = RecoverySendBuffer::new(100);
        buf.push(1, make_payload(3)).unwrap();
        buf.push(2, make_payload(4)).unwrap();
        let initial_len = buf.len();
        assert_eq!(initial_len, 2);

        // The pre-fix code would have walked both items, then returned
        // NotFound. After the fix `u64::MAX` is rejected up front, but
        // crucially the buffer must NOT be mutated as a side effect.
        let err = buf.skip(u64::MAX).unwrap_err();
        let RecoveryError::NotFound { requested } = err;
        assert_eq!(requested, u64::MAX);
        assert_eq!(
            buf.len(),
            initial_len,
            "skip(u64::MAX) must not consume buffered items"
        );
    }

    #[test]
    fn head_seq_and_tail_seq_reflect_window() {
        let mut buf = RecoverySendBuffer::new(100);
        assert_eq!(buf.head_seq(), 0);
        assert_eq!(buf.tail_seq(), 0);

        buf.push(5, make_payload(3)).unwrap();
        buf.push(6, make_payload(3)).unwrap();
        buf.push(7, make_payload(3)).unwrap();
        assert_eq!(buf.head_seq(), 5);
        assert_eq!(buf.tail_seq(), 7);
    }
}
