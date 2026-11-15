#!/usr/bin/env bash
# Headless end-to-end test on Linux: Xvfb + Mesa lavapipe (software Vulkan),
# mock xCloud server, Nimbus auto-streaming and saving a screenshot to
# target/deck/e2e.png. Runs inside the build container (mise run deck:e2e).
set -euo pipefail
cd "$(dirname "$0")/../.."
MEDIA=target/mock-media
OUT=target/deck/e2e.png
TARGET="${CARGO_TARGET_DIR:-target}"
mise install
mise run build
mise exec -- cargo build --release -p xcloud --example mock_server
mkdir -p target/deck
"$TARGET/release/examples/mock_server" "$MEDIA" 8085 > /tmp/mock.log 2>&1 &
MOCK=$!
trap 'kill $MOCK 2>/dev/null || true' EXIT
sleep 1
mkdir -p /tmp/nimbus-home/config
echo '{"fullscreen_on_stream":false,"show_stats":true}' > /tmp/nimbus-home/config/settings.json
SDL_AUDIO_DRIVER=dummy RUST_LOG=info NIMBUS_HOME=/tmp/nimbus-home NIMBUS_MOCK=http://127.0.0.1:8085 \
  NIMBUS_AUTOSTREAM=home NIMBUS_TEST_INPUT=1 NIMBUS_SCREENSHOT="$OUT,25" NIMBUS_EXIT_AFTER=30 \
  xvfb-run -a -s "-screen 0 1280x800x24" "$TARGET/release/nimbus" 2>&1 | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E "decoder ready|handshake|stream stats|screenshot|ERROR|panic" || true
echo "--- mock server"
sed 's/\x1b\[[0-9;]*m//g' /tmp/mock.log | grep -E "connected|gamepad 0|stats|deleted" | tail -6
test -s "$OUT" && echo "OK: $OUT"
