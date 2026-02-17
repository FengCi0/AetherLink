#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-}"

if [[ -z "${MODE}" ]]; then
  echo "usage: $0 <receiver|sender|demo>"
  exit 1
fi

if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is required"
  exit 1
fi

PORT="${AETHERLINK_MEDIA_PORT:-5600}"
FPS="${AETHERLINK_MEDIA_FPS:-15}"
SIZE="${AETHERLINK_MEDIA_SIZE:-1280x720}"
DISPLAY_INPUT="${AETHERLINK_MEDIA_DISPLAY:-:0.0}"
OFFSET="${AETHERLINK_MEDIA_OFFSET:-0,0}"
TARGET_IP="${AETHERLINK_MEDIA_TARGET_IP:-127.0.0.1}"
DEMO_SECONDS="${AETHERLINK_MEDIA_DEMO_SECONDS:-10}"

run_receiver() {
  echo "[receiver] listening on udp://0.0.0.0:${PORT}"
  ffmpeg -hide_banner -loglevel info \
    -fflags nobuffer -flags low_delay \
    -i "udp://0.0.0.0:${PORT}?fifo_size=1000000&overrun_nonfatal=1" \
    -an -f null -
}

run_sender() {
  echo "[sender] capture=${DISPLAY_INPUT}+${OFFSET} size=${SIZE} fps=${FPS} target=${TARGET_IP}:${PORT}"
  ffmpeg -hide_banner -loglevel info \
    -f x11grab \
    -framerate "${FPS}" \
    -video_size "${SIZE}" \
    -i "${DISPLAY_INPUT}+${OFFSET}" \
    -an \
    -c:v libx264 \
    -preset ultrafast \
    -tune zerolatency \
    -pix_fmt yuv420p \
    -g "${FPS}" \
    -f mpegts "udp://${TARGET_IP}:${PORT}?pkt_size=1200"
}

run_demo() {
  echo "[demo] starting local receiver + sender for ${DEMO_SECONDS}s"
  run_receiver >/tmp/aetherlink-media-receiver.log 2>&1 &
  RECEIVER_PID=$!
  trap 'kill ${RECEIVER_PID} >/dev/null 2>&1 || true' EXIT
  sleep 1

  timeout "${DEMO_SECONDS}"s bash -lc "$(declare -f run_sender); run_sender" || true
  sleep 1

  echo
  echo "receiver log tail:"
  tail -n 20 /tmp/aetherlink-media-receiver.log || true
}

case "${MODE}" in
  receiver)
    run_receiver
    ;;
  sender)
    run_sender
    ;;
  demo)
    run_demo
    ;;
  *)
    echo "unknown mode: ${MODE}"
    echo "usage: $0 <receiver|sender|demo>"
    exit 1
    ;;
esac
