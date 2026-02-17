#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

. "$HOME/.cargo/env"

echo "[1/3] Building..."
cargo build -p aetherlink-node >/dev/null

echo "[2/3] Starting node A..."
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9901/quic-v1 \
  > /tmp/aetherlink-node-a.log 2>&1 &
PID_A=$!

cleanup() {
  kill "$PID_A" "$PID_B" >/dev/null 2>&1 || true
}
trap cleanup EXIT

sleep 1

echo "[3/3] Starting node B (auto-request)..."
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9902/quic-v1 \
  --dial /ip4/127.0.0.1/udp/9901/quic-v1 \
  --auto-request \
  > /tmp/aetherlink-node-b.log 2>&1 &
PID_B=$!

sleep 4

echo
echo "Node A log excerpt:"
tail -n 20 /tmp/aetherlink-node-a.log || true
echo
echo "Node B log excerpt:"
tail -n 20 /tmp/aetherlink-node-b.log || true
echo
echo "Expected markers:"
echo "  - sent SessionRequest"
echo "  - received SessionRequest"
echo "  - session accepted"
