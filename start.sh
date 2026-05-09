#!/usr/bin/env bash
# chaplin-click — lip-read to screen-click bridge.
#
# Usage: ./start.sh [--dry-run] [--verbose] [--fast]
#   All flags are forwarded to the chaplin-click binary.
#
# Prerequisites:
#   - User must be in 'input' group (for evdev access)
#   - Ollama running with qwen3:4b pulled
#   - Qwen2.5-VL server on :8082 (for screen-click)
#   - ydotoold running (for screen-click)
#   - Webcam accessible at /dev/video0
set -euo pipefail

cd "$(dirname "$0")"

BIN="./target/release/chaplin-click"

if [[ ! -x "$BIN" ]]; then
  echo "Building chaplin-click..."
  cargo build --release
fi

exec "$BIN" "$@"
