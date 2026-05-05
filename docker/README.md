# UDS Tunnel Server — Docker Deployment Guide

## Overview

This Docker setup provides a deployment of the UDS Tunnel Server.
It uses **two separate Dockerfiles** for a clean separation between build and production:

| Image | Base | Purpose |
|-------|------|---------|
| `Dockerfile.build` | `debian:trixie` | Compiles Rust tunnel-server binary |
| `Dockerfile` | `debian:trixie-slim` | Production image |

Both Dockerfiles use `ARG DISTRO_VERSION=trixie` so you can change the base distro in the future
with a single `--build-arg DISTRO_VERSION=forky`.

## Architecture

```
                  ┌────────────── Docker Container ──────────────┐
                  │                                               │
   Host:4443 ───►│  udstunnel :4443 (TCP/TLS)                    │
                  │                                               │
                  └───────────────────────────────────────────────┘
```

- **udstunnel** listens directly on the configured port (default 4443).
- Logs are stored in `/var/log/udstunnel` (should be mounted as a volume for persistence).

## Quick Start

### 1. Build the Builder Image

```bash
cd /path/to/tunnel-server

docker build -f docker/Dockerfile.build -t udstunnel-builder .
```

This compiles the Rust server.

### 2. Build the Production Image

```bash
docker build -f docker/Dockerfile \
  --build-arg BUILDER_IMAGE=udstunnel-builder \
  -t udstunnel .
```

### 3. Prepare Configuration

Copy the example configuration:

```bash
cp docker/udstunnel.conf.example udstunnel.conf
```

Edit `udstunnel.conf` with your broker details and token.

### 4. Access

```bash
docker run -d \
  --name udstunnel \
  -p 4443:4443 \
  -v $(pwd)/udstunnel.conf:/etc/udstunnel.conf:ro \
  -v /path/to/logs:/var/log/udstunnel \
  udstunnel
```

## Configuration Reference

### Docker Volumes

| Container Path | Description | Required |
|----------------|-------------|----------|
| `/etc/udstunnel.conf` | Server configuration file | Recommended |
| `/var/log/udstunnel` | Log directory | Recommended (for persistence) |

### Port Mapping

| Container Port | Protocol | Description |
|----------------|----------|-------------|
| `4443` | TCP | Tunnel Server Listener |

Typical mapping: `-p 4443:4443`

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `UDSTUNNEL_TUNNEL_LOG_PATH` | `/var/log/udstunnel` | Directory where logs and panic logs are stored. |

## Troubleshooting

### Server fails to start
- Check logs: `docker logs udstunnel`
- Verify the config file is mounted correctly at `/etc/udstunnel.conf`.

### Panic Logs
- If the server crashes unexpectedly, check `udstunnel-panic.log` in the log directory.
