#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKET_PATH="${AETHERLINK_DAEMON_SOCKET:-/tmp/aetherlink-daemon-demo.sock}"

source "$HOME/.cargo/env"

cleanup() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" >/dev/null 2>&1; then
    kill "$DAEMON_PID" >/dev/null 2>&1 || true
    wait "$DAEMON_PID" >/dev/null 2>&1 || true
  fi
  rm -f "$SOCKET_PATH"
}
trap cleanup EXIT

echo "[demo] build binaries..."
cargo build -p aetherlink-daemon -p aetherlink-daemonctl -p aetherlink-node >/dev/null

echo "[demo] starting daemon..."
"$ROOT_DIR/target/debug/aetherlink-daemon" \
  --socket-path "$SOCKET_PATH" \
  > /tmp/aetherlink-daemon-demo.log 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 30); do
  [[ -S "$SOCKET_PATH" ]] && break
  sleep 0.1
done

echo "[demo] start managed node via daemonctl..."
"$ROOT_DIR/target/debug/aetherlink-daemonctl" \
  --socket-path "$SOCKET_PATH" \
  start \
  --node-binary "$ROOT_DIR/target/debug/aetherlink-node" \
  --listen "/ip4/0.0.0.0/udp/9100/quic-v1" \
  --trust-on-first-use false

echo "[demo] discover known devices..."
"$ROOT_DIR/target/debug/aetherlink-daemonctl" \
  --socket-path "$SOCKET_PATH" \
  discover

echo "[demo] stop managed node..."
"$ROOT_DIR/target/debug/aetherlink-daemonctl" \
  --socket-path "$SOCKET_PATH" \
  stop

echo "[demo] done."
