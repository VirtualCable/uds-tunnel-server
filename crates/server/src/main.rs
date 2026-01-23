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
// Authors: Adolfo Gómez, dkmaster at dkmon dot compub mod broker;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio::{net::TcpListener, signal};

pub mod broker;
pub mod config;
pub mod connection;
pub mod consts;
pub mod crypt;
pub mod errors;
pub mod log;
pub mod session;
pub mod stream;
pub mod system;
pub mod ticket;
pub mod utils;

// Catch SIGTERM and SIGINT to perform a graceful shutdown
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    log::setup_logging("debug", log::LogType::Tunnel);
    // Crate a listener with the configured address
    let (listen_sock_addr, use_proxy_protocol) = {
        let config = config::get();
        let config = config.read().unwrap();
        (
            config.listen_sockaddr(),
            config.use_proxy_protocol.unwrap_or(false),
        )
    };
    let listener = TcpListener::bind(listen_sock_addr).await.unwrap();
    log::info!("Listening on {}", listen_sock_addr);

    let stop = system::trigger::Trigger::new();

    // Spawn the signal handler
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            let ctrl_c = signal::ctrl_c();
            #[cfg(unix)]
            let mut terminate =
                unix_signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");

            #[cfg(unix)]
            tokio::select! {
                    _ = ctrl_c => {
                        log::info!("Received Ctrl-C, shutting down");
                    }
                    _ = terminate.recv() => {
                        log::info!("Received SIGTERM, shutting down");
                }
            }
            #[cfg(not(unix))]
            {
                ctrl_c.await.expect("Failed to listen for Ctrl-C");
                log::info!("Received Ctrl-C, shutting down");
            }
            session::SessionManager::get_instance()
                .finish_all_sessions()
                .await;
            stop.trigger();
        });
    }

    loop {
        tokio::select! {
            _ = stop.wait_async() => {
                log::info!("Shutdown signal received, stopping listener");
                break;
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((socket, addr)) => {
                        log::info!("Accepted connection from {}", addr);
                        tokio::spawn({
                            let (reader, writer) = socket.into_split();
                            async move {
                                if let Err(e) =
                                    connection::handle_connection(reader, writer, addr, use_proxy_protocol).await
                                {
                                    log::error!("Error handling connection from {}: {:?}", addr, e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("Failed to accept connection: {:?}", e);
                    }
                }
            }
        }
    }
}
