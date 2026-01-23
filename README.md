# UDS Tunnel Server

## Overview

The UDS Tunnel Server is a high-performance tunneling service written in Rust that enables secure connections between clients and backend services through authenticated ticket-based access control.

## Features

- **Secure Tunneling**: Establishes encrypted tunnels between clients and target servers
- **Ticket-Based Authentication**: Uses broker-validated tickets for connection authorization
- **TLS Support**: Built-in TLS encryption for secure communications
- **Proxy Protocol Support**: Optional PROXY protocol v2 support for load balancers
- **Asynchronous I/O**: High-performance async Rust implementation using Tokio
- **Graceful Shutdown**: Proper signal handling for clean shutdowns

## Configuration

The server is configured via a TOML configuration file (`udstunnel.conf` in debug mode, `/etc/udstunnel.conf` in release mode).

### Configuration Options

```toml
# Network settings
listen_addr = "*"          # Listen address (* for all interfaces)
listen_port = 4443         # Listen port (default: 443)
use_proxy_protocol = false # Enable PROXY protocol v2 (default: false)

# Broker API settings
ticket_api_url = "https://broker.example.com/uds/rest/tunnel/ticket"
verify_ssl = true           # Verify SSL certificates (default: true)
broker_auth_token = "your_auth_token"
```

## Architecture

### Components

- **Connection Handler**: Manages incoming TCP connections and performs handshake
- **Broker API Client**: Validates tickets with the UDS broker service
- **Session Manager**: Manages active tunnel sessions
- **Stream Handler**: Manages bidirectional data flow between client and target

### Handshake Process

1. Client connects and sends handshake with ticket
2. Server validates ticket via broker API
3. If valid, server establishes connection to target host
4. Bidirectional tunnel is established

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
- Tunnel is encrypted using a pre-shared key, used with the ticket to derive session keys
- The pre-shared key has been shared previously between the broker/client using ml-kyber and using TLS with tunnel.

## Version

Current version: 5.0.0

## License

BSD 3-Clause License</content>
<parameter name="filePath">/home/dkmaster/projects/uds/5.0/repos/tunnel/tunnel-server.md