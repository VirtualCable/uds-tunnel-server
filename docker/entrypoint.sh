#!/bin/bash
# =============================================================================
# UDS Tunnel Server — Docker Entrypoint
# =============================================================================
set -e

# Default log directory
LOG_DIR="/var/log/udstunnel"
CONFIG_FILE="/etc/udstunnel.conf"
SERVER_BIN="/opt/udstunnel/udstunnel"

# --- Set Log Environment Variable ---
# This ensures that the server writes logs to the mounted volume
export UDSTUNNEL_TUNNEL_LOG_PATH="${UDSTUNNEL_TUNNEL_LOG_PATH:-$LOG_DIR}"

# Create log directory if it doesn't exist
if [ ! -d "$UDSTUNNEL_TUNNEL_LOG_PATH" ]; then
    echo "[entrypoint] Creating log directory: $UDSTUNNEL_TUNNEL_LOG_PATH"
    mkdir -p "$UDSTUNNEL_TUNNEL_LOG_PATH"
fi

# --- Validate configuration file ---
if [ ! -f "${CONFIG_FILE}" ]; then
    echo "[entrypoint] WARNING: Configuration file not found at ${CONFIG_FILE}"
    echo "[entrypoint] You should mount your config with: -v /path/to/udstunnel.conf:${CONFIG_FILE}:ro"
    echo "[entrypoint] Starting with internal defaults..."
fi

# --- Start the backend server ---
echo "[entrypoint] Starting udstunnel..."
${SERVER_BIN} &
SERVER_PID=$!

# Give the server a moment to start
sleep 1

# Check if server is still running
if ! kill -0 ${SERVER_PID} 2>/dev/null; then
    echo "[entrypoint] ERROR: Server failed to start. Check configuration."
    exit 1
fi

echo "[entrypoint] Server started (PID: ${SERVER_PID})"

# --- Handle graceful shutdown ---
cleanup() {
    echo "[entrypoint] Shutting down..."
    kill -TERM ${SERVER_PID} 2>/dev/null || true
    wait ${SERVER_PID} 2>/dev/null || true
    echo "[entrypoint] Shutdown complete."
    exit 0
}
trap cleanup SIGTERM SIGINT SIGQUIT

# Wait for the process to exit
wait ${SERVER_PID} 2>/dev/null || true

# If process exited, clean up
echo "[entrypoint] Server process exited."
cleanup
