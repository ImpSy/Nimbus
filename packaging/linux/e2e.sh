#!/usr/bin/env bash
# Headless end-to-end test on Linux: Xvfb + Mesa lavapipe (software Vulkan),
# mock xCloud server, Nimbus auto-streaming and taking a screenshot.
#   docker run --rm --platform linux/amd64 -v "$PWD":/src -v nimbus-cargo:/root/.cargo/registry \
#     -v nimbus-target:/src/target nimbus-build bash packaging/linux/e2e.sh <media_dir> <out.png>
set -euo pipefail
cd "$(dirname "$0")/../.."
MEDIA="${1:-target/mock-media}"
OUT="${2:-target/linux-e2e.png}"
apt-get update -qq && apt-get install -y -qq xvfb mesa-vulkan-drivers libvulkan1 >/dev/null
cargo build --release -p nimbus --features static-ffmpeg-linux
cargo build --release -p xcloud --example mock_server
target/release/examples/mock_server "$MEDIA" 8085 > /tmp/mock.log 2>&1 &
MOCK=$!
sleep 1
mkdir -p /tmp/nimbus-home/config
echo '{"fullscreen_on_stream":false,"show_stats":true}' > /tmp/nimbus-home/config/settings.json
SDL_AUDIO_DRIVER=dummy RUST_LOG=info NIMBUS_HOME=/tmp/nimbus-home NIMBUS_MOCK=http://127.0.0.1:8085 \
  NIMBUS_AUTOSTREAM=home NIMBUS_TEST_INPUT=1 NIMBUS_SCREENSHOT="$OUT,25" NIMBUS_EXIT_AFTER=30 \
  xvfb-run -a -s "-screen 0 1280x800x24" target/release/nimbus 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | tail -40 || true
kill $MOCK || true
echo "--- mock server"
sed 's/\x1b\[[0-9;]*m//g' /tmp/mock.log | grep -E "connected|handshake|gamepad 0|stats|deleted" | tail -8
ls -la "$OUT"
