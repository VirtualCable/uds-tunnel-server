# UDS Tunnel Server

## Overview

The UDS Tunnel Server is a high-performance tunneling service written in Rust that enables secure connections between clients and backend services through authenticated ticket-based access control.

## Features

- **Secure Tunneling**: Establishes encrypted tunnels between clients and target servers
- **Ticket-Based Authentication**: Uses broker-validated tickets for connection authorization
- **Encrypted Tunnels**: Per-frame AES-256-GCM AEAD on the tunnel leg (TLS is used only towards the broker API)
- **Per-Epoch Key Rotation (Rekeying)**: the tunnel re-derives its AES-256-GCM key every `2^k` frames per direction (default `k = 20`, configurable), bounding AEAD invocations per key (NIST SP 800-38D). Deterministic (no rekey handshake, no transition window); negotiated once per session in the `OpenResponse` — see `docs/rekeying-contract.md`.
- **UDP Relay**: Optional parallel UDP leg (e.g. RDPUDP redirection) with per-datagram AEAD encryption
- **Proxy Protocol Support**: Optional PROXY protocol v2 support for load balancers
- **Asynchronous I/O**: High-performance async Rust implementation using Tokio
- **Graceful Shutdown**: Proper signal handling for clean shutdowns

## Configuration

The server is configured via a TOML configuration file (`udstunnel.conf` in debug mode, `/etc/udstunnel.conf` in release mode).

### Configuration Options

```toml
# Network settings
listen_addr = "*"          # Listen address (* for all interfaces)
listen_port = 4443         # Listen port (built-in default: 443)
use_proxy_protocol = false # Enable PROXY protocol v2 (default: false)

# UDP relay (RDP UDP redirection)
udp_enabled = true         # Bind the shared UDP relay socket (default: true)
# udp_listen_port = 4443   # UDP relay port (default: same as listen_port)

# Broker API settings
ticket_api_url = "https://broker.example.com/uds/rest/tunnel/ticket"
dangerous_disable_ssl_verify = false  # Verify SSL certificates (default: false). Only enable for diagnostics against self-signed brokers.
broker_auth_token = "your_auth_token"

# Sessions
recovery_buffer_size = 64              # Per-session recovery buffer, KB (default: 64)
max_sessions = 8192                    # Concurrent session cap (default: 8192)
# max_sessions_per_remote = 128        # Per-source-IP cap (default: disabled)
session_idle_data_timeout_secs = 120   # Data-idle session cap, seconds (0 = disabled)

# Key rotation (rekeying)
rekey_seq_log2 = 20                    # log2 of frames per AES-GCM key epoch (0 = OFF, 1..=63)

# Logging
log_level = "info"                     # trace | debug | info | warn | error
```

Every field, its default and the environment overrides are documented in
[`docs/config.md`](docs/config.md).

## Architecture

### Components

- **Connection Handler**: Manages incoming TCP connections and performs handshake
- **Broker API Client**: Validates tickets with the UDS broker service
- **Session Manager**: Manages active tunnel sessions
- **Stream Handler**: Manages bidirectional data flow between client and target

### Handshake Process

1. Client (launcher) connects and sends the handshake: ticket + a post-quantum
   ML-KEM public key.
2. The server resolves the ticket: the broker's encrypted response carries the
   matching ML-KEM ciphertext, and both sides derive the same shared secret.
3. The server registers the session, then waits for the launcher to echo the
   ticket back, AEAD-encrypted under the epoch-0 keys (post-handshake confirm).
4. On a matching echo the server replies with an `OpenResponse` (session id,
   channel count, per-direction sequence counters, UDP token/port,
   `rekey_log2`), encrypted under the same epoch-0 keys.
5. The bidirectional tunnel is established; the launcher opens one data
   channel per remote via `OpenChannel`.

## Building

```bash
cargo build --release
```

## Running

```bash
# In debug mode (uses udstunnel.conf)
cargo run --bin tunnel-server

# In release mode (uses /etc/udstunnel.conf)
./target/release/tunnel-server
```

## Dependencies

- **tokio**: Async runtime
- **rustls**: TLS implementation
- **reqwest**: HTTP client for broker communication
- **serde**: Serialization
- **tracing**: Logging

## Security

- HTTPS communication with broker API
- Tunnel is encrypted using a shared secret, used with the ticket to derive the session keys
- The shared secret is established with post-quantum ML-KEM: the launcher sends its public key with the ticket, the broker returns the matching ciphertext, and both sides derive the same secret. The tunnel itself has no TLS layer (AES-256-GCM per frame).

### UDP leg

When the broker flags a ticket with `enable_udp` (tunneled RDP transports), the server accepts a parallel UDP relay leg on `udp_listen_port` (defaults to the TCP port):

- Each datagram is AEAD-encrypted (AES-256-GCM) with dedicated keys derived via HKDF (`openuds-ticket-crypt-udp` label) from the same ML-KEM ticket secret — domain-separated from the TCP leg keys, keeping the post-quantum property end to end.
- Datagrams are demultiplexed to sessions by a random 128-bit per-session token assigned at `Open` time.
- Anti-replay uses a 1024-entry sliding-window bitmap (IPsec/DTLS style) instead of the strict counter of the TCP leg, tolerating reordering and loss; sequence numbers start at 2^63 so they cannot wrap.
- The relay is deliberately unreliable (no reordering, no retransmission): RDPUDP implements its own reliability, and the session falls back to TCP-only (`E_ABORT`) if UDP never establishes.
- Anti-amplification: no UDP traffic is ever sent to a client address that has not first sent an AEAD-authenticated datagram.

## Version

Current version: 5.0.0

## License

BSD 3-Clause License