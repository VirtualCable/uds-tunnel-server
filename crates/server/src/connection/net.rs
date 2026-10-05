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
//
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Raw-socket options applied to accepted launcher connections.

use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;

use shared::log;

/// OS-level TCP keepalive backup for the launcher->server leg.
///
/// The application-level keep-alive is the primary liveness mechanism (the
/// inbound stream tears the leg down after `KEEPALIVE_TIMEOUT_SECS` without
/// any decrypted frame), but it only reacts once the TCP layer surfaces an
/// error (RST/FIN) or a frame simply stops arriving. On a *silently*
/// black-holed route (dead NAT entry, yanked cable without RST) the read
/// half never errors and the write half buffers until it fills; SO_KEEPALIVE
/// makes the kernel probe the peer so the dead socket eventually errors on
/// **both** stacks, which frees the session promptly and lets the launcher
/// notice and reconnect from its side too.
///
/// Probing is strictly backup timing: it starts after the application
/// deadline has passed, so it never pre-empts the normal recovery path
/// (which requires no TCP error: `Recover` works on the grace window).
///
/// Failures are non-fatal (best-effort): platforms or middleboxes that
/// reject the options keep working with the application-level keep-alive
/// alone.
// Probe a leg that has been silent at the TCP layer only: the launcher
// sends keep-alive frames every 2s, so on a healthy connection this
// timer never expires before the application deadline does.
const IDLE_SECS: u64 = 15;
const INTERVAL_SECS: u64 = 5;
const RETRIES: u32 = 3;

pub fn set_keepalive(stream: &TcpStream) {
    let ka = TcpKeepalive::new()
        .with_time(Duration::from_secs(IDLE_SECS))
        .with_interval(Duration::from_secs(INTERVAL_SECS));
    let sock = SockRef::from(stream);
    // `with_retries` (TCP_KEEPCNT) is Unix-only; on Windows the count is not
    // configurable per-socket and the defaults are acceptable for a backup.
    #[cfg(unix)]
    let ka = ka.with_retries(RETRIES);

    if let Err(e) = sock.set_tcp_keepalive(&ka) {
        log::debug!(
            "Failed to set TCP keepalive on launcher connection: {:?}",
            e
        );
    }

    // TCP_USER_TIMEOUT is the option that actually bounds a *black-holed*
    // leg: SO_KEEPALIVE's idle timer restarts on every (unacknowledged)
    // write, and the keep-alive frames themselves are writes, so a one-way
    // data black hole can keep the socket "alive" indefinitely. This option
    // fails the socket once transmitted data stays unacknowledged for the
    // given time, regardless of how often we write. Linux-only; elsewhere
    // SO_KEEPALIVE above is the best available backup.
    #[cfg(target_os = "linux")]
    if let Err(e) = sock.set_tcp_user_timeout(Some(Duration::from_secs(
        IDLE_SECS + INTERVAL_SECS * RETRIES as u64,
    ))) {
        log::debug!(
            "Failed to set TCP_USER_TIMEOUT on launcher connection: {:?}",
            e
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_keepalive` must actually apply the options to a real TCP socket
    /// (the accept-loop path), reading them back from the kernel.
    #[tokio::test]
    async fn keepalive_options_are_applied_to_a_real_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::net::TcpStream::connect(addr);
        let accept = listener.accept();
        let (client, (server, _)) = tokio::try_join!(connect, accept).unwrap();

        for stream in [&server, &client] {
            set_keepalive(stream);
            let sock = socket2::SockRef::from(stream);
            assert!(sock.keepalive().unwrap(), "SO_KEEPALIVE must be on");
            assert_eq!(sock.tcp_keepalive_time().unwrap(), Duration::from_secs(15));
            assert_eq!(
                sock.tcp_keepalive_interval().unwrap(),
                Duration::from_secs(5)
            );
            #[cfg(unix)]
            assert_eq!(sock.tcp_keepalive_retries().unwrap(), 3);
            #[cfg(target_os = "linux")]
            assert_eq!(
                sock.tcp_user_timeout().unwrap(),
                Some(Duration::from_secs(15 + 5 * 3))
            );
        }
    }
}
