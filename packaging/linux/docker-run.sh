#!/usr/bin/env bash
# Run a script inside the Linux build container (x86_64, like the Steam Deck).
# Toolchains, cargo caches and build output live in named volumes; the repo is
# mounted at /src so results (target/deck, .deps/) land in the working tree.
set -euo pipefail
cd "$(dirname "$0")/../.."
exec docker run --rm --platform linux/amd64 \
  -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
  -v "$PWD":/src \
  -v nimbus-mise:/root/.local/share/mise \
  -v nimbus-rustup:/root/.rustup \
  -v nimbus-cargo:/root/.cargo \
  -v nimbus-build:/build \
  nimbus-build bash "$@"
