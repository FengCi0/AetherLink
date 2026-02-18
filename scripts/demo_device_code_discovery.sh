#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

. "$HOME/.cargo/env"

LOG_SEED=/tmp/aetherlink-discovery-seed.log
LOG_A=/tmp/aetherlink-discovery-a.log
LOG_B=/tmp/aetherlink-discovery-b.log
KEY_SEED=/tmp/aetherlink-discovery-seed.key
KEY_A=/tmp/aetherlink-discovery-a.key
KEY_B=/tmp/aetherlink-discovery-b.key
TRUST_SEED=/tmp/aetherlink-discovery-seed-trust.json
TRUST_A=/tmp/aetherlink-discovery-a-trust.json
TRUST_B=/tmp/aetherlink-discovery-b-trust.json

rm -f "$LOG_SEED" "$LOG_A" "$LOG_B"

echo "[1/5] Building..."
cargo build -p aetherlink-node >/dev/null

echo "[2/5] Starting seed node..."
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9910/quic-v1 \
  --identity-file "$KEY_SEED" \
  --trust-store-file "$TRUST_SEED" \
  --trust-on-first-use true \
  >"$LOG_SEED" 2>&1 &
PID_SEED=$!

echo "    waiting for seed peer id..."
PEER_SEED=""
for _ in $(seq 1 40); do
  PEER_SEED="$(sed -n 's/.*local peer id: \([^,]*\),.*/\1/p' "$LOG_SEED" | tail -n 1)"
  if [[ -n "$PEER_SEED" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -z "$PEER_SEED" ]]; then
  echo "failed to parse seed peer id"
  exit 1
fi

echo "    seed peer id: $PEER_SEED"

echo "[3/5] Starting node A (target)..."
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9911/quic-v1 \
  --identity-file "$KEY_A" \
  --trust-store-file "$TRUST_A" \
  --trust-on-first-use true \
  --bootstrap /ip4/127.0.0.1/udp/9910/quic-v1/p2p/"$PEER_SEED" \
  >"$LOG_A" 2>&1 &
PID_A=$!

PID_B=""
cleanup() {
  kill "$PID_SEED" >/dev/null 2>&1 || true
  kill "$PID_A" >/dev/null 2>&1 || true
  if [[ -n "$PID_B" ]]; then
    kill "$PID_B" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

echo "    waiting for node A peer id..."
PEER_A=""
for _ in $(seq 1 40); do
  PEER_A="$(sed -n 's/.*local peer id: \([^,]*\),.*/\1/p' "$LOG_A" | tail -n 1)"
  if [[ -n "$PEER_A" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -z "$PEER_A" ]]; then
  echo "failed to parse node A peer id"
  exit 1
fi

echo "    node A device code: $PEER_A"

echo "[4/5] Starting node B (device-code discovery + auto-request)..."
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9912/quic-v1 \
  --identity-file "$KEY_B" \
  --trust-store-file "$TRUST_B" \
  --trust-on-first-use true \
  --bootstrap /ip4/127.0.0.1/udp/9910/quic-v1/p2p/"$PEER_SEED" \
  --connect-device-code "$PEER_A" \
  --auto-request \
  >"$LOG_B" 2>&1 &
PID_B=$!

echo "[5/5] Waiting for discovery and handshake..."
sleep 8

echo
echo "Seed log excerpt:"
tail -n 20 "$LOG_SEED" || true
echo
echo "Node A log excerpt:"
tail -n 30 "$LOG_A" || true
echo
echo "Node B log excerpt:"
tail -n 40 "$LOG_B" || true
echo
echo "Expected markers:"
echo "  - published local device announcement to DHT"
echo "  - started DHT device lookup"
echo "  - device discovery resolved target="
echo "  - sent SessionRequest"
echo "  - session accepted"
