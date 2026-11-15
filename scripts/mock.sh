#!/usr/bin/env bash
# Run Nimbus against the local mock xCloud server (started in the background).
# Extra NIMBUS_* variables are passed through (e.g. NIMBUS_AUTOSTREAM=home).
set -euo pipefail
cd "$(dirname "$0")/.."
PORT="${MOCK_PORT:-8085}"
cargo build -p xcloud --example mock_server
cargo build --release -p nimbus
target/debug/examples/mock_server target/mock-media "$PORT" &
MOCK=$!
trap 'kill $MOCK 2>/dev/null || true' EXIT
sleep 0.5
NIMBUS_MOCK="http://127.0.0.1:$PORT" target/release/nimbus
