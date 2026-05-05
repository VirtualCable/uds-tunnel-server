# =============================================================================
# UDS Tunnel Server — Production Image
# =============================================================================
# Lightweight production image for the UDS Tunnel Server.
#
# Prerequisites:
#   docker build -f docker/Dockerfile.build -t udstunnel-builder .
#
# Usage:
#   docker build -f docker/Dockerfile \
#     --build-arg BUILDER_IMAGE=udstunnel-builder \
#     -t udstunnel .
#
# Run:
#   docker run -d -p 4443:4443 \
#     -v /path/to/udstunnel.conf:/etc/udstunnel.conf:ro \
#     -v /path/to/logs:/var/log/udstunnel \
#     udstunnel
# =============================================================================

ARG DISTRO_VERSION=trixie
ARG BUILDER_IMAGE=udstunnel-builder

# Pull artifacts from the builder image
FROM ${BUILDER_IMAGE} AS builder

# Production image — slim variant for minimal footprint
FROM debian:${DISTRO_VERSION}-slim

LABEL maintainer="Virtual Cable S.L. <support@virtualcable.net>"
LABEL description="UDS Tunnel Server — Production"

# Install runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl ca-certificates \
    libssl3t64 \
    && rm -rf /var/lib/apt/lists/*

# Create directories
RUN mkdir -p /opt/udstunnel /var/log/udstunnel

# Copy compiled server binary
COPY --from=builder /build/target/release/tunnel-server /opt/udstunnel/udstunnel
RUN chmod +x /opt/udstunnel/udstunnel

# Copy entrypoint script
COPY docker/entrypoint.sh /opt/udstunnel/entrypoint.sh
RUN chmod +x /opt/udstunnel/entrypoint.sh

# The server listens on 4443 by default
EXPOSE 4443

ENTRYPOINT ["/opt/udstunnel/entrypoint.sh"]
