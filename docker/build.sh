#!/bin/bash
# =============================================================================
# UDS Tunnel Server — Docker Build Script
# =============================================================================
# Builds both the builder and production images.
#
# Usage:
#   ./docker/build.sh              # Build both images
#   ./docker/build.sh --no-cache   # Build without Docker cache
# =============================================================================
set -e

# Navigate to project root (parent of docker/)
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_ROOT"

# Image names
BUILDER_IMAGE="udstunnel-builder"
PRODUCTION_IMAGE="udstunnel"

# Forward any extra args (e.g. --no-cache) to docker build
EXTRA_ARGS="$@"

echo "=============================================="
echo "  UDS Tunnel Server — Docker Build"
echo "=============================================="
echo "  Project root: ${PROJECT_ROOT}"
echo "  Builder image: ${BUILDER_IMAGE}"
echo "  Production image: ${PRODUCTION_IMAGE}"
echo "=============================================="
echo ""

echo "[1/2] Building builder image (${BUILDER_IMAGE})..."
echo "      This compiles the Rust tunnel-server binary."
echo ""
docker build \
    -f docker/Dockerfile.build \
    -t "${BUILDER_IMAGE}" \
    ${EXTRA_ARGS} \
    .

echo ""
echo "      ✓ Builder image ready."
echo ""

echo "[2/2] Building production image (${PRODUCTION_IMAGE})..."
echo "      This creates the final server image."
echo ""
docker build \
    -f docker/Dockerfile \
    --build-arg BUILDER_IMAGE="${BUILDER_IMAGE}" \
    -t "${PRODUCTION_IMAGE}" \
    ${EXTRA_ARGS} \
    .

echo ""
echo "=============================================="
echo "  ✓ Build complete!"
echo "=============================================="
echo ""
echo "  Run with:"
echo "    docker run -d -p 4443:4443 \\"
echo "      -v \$(pwd)/udstunnel.conf:/etc/udstunnel.conf:ro \\"
echo "      -v \$(pwd)/logs:/var/log/udstunnel \\"
echo "      ${PRODUCTION_IMAGE}"
echo ""
