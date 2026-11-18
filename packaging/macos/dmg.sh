#!/usr/bin/env bash
# Build target/Nimbus-<version>-macos-arm64.dmg (Nimbus.app + an Applications
# shortcut for drag-to-install). Prints the DMG path as the last line.
# Usage: mise run dmg:macos
set -euo pipefail
cd "$(dirname "$0")/../.."

packaging/macos/bundle.sh >&2  # keep stdout for the DMG path

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
DMG="target/Nimbus-${VERSION}-macos-arm64.dmg"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R target/Nimbus.app "$STAGE/"
ln -s /Applications "$STAGE/Applications"

hdiutil create -quiet -volname "Nimbus" -srcfolder "$STAGE" \
  -fs HFS+ -format UDZO -ov "$DMG"
echo "$DMG"
