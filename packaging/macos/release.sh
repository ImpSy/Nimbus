#!/usr/bin/env bash
# Build the macOS DMG and publish it as GitHub release v<version> (version from
# Cargo.toml), with notes generated from the PRs/commits since the last release.
# The tag is created on GitHub at the current commit, which must be pushed.
# Extra arguments go to `gh release create`, e.g. --draft or --prerelease.
# Usage: mise run release:macos [--draft]
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
TAG="v${VERSION}"
COMMIT=$(git rev-parse HEAD)

# Fail fast, before the (slow) build.
if gh release view "$TAG" >/dev/null 2>&1; then
  echo "Release $TAG already exists: bump the version in Cargo.toml first." >&2
  exit 1
fi
if [[ -n $(git status --porcelain) ]]; then
  echo "Working tree has uncommitted changes: commit or discard them first." >&2
  exit 1
fi
git fetch --quiet origin
if [[ -z $(git branch -r --contains "$COMMIT") ]]; then
  echo "Commit ${COMMIT:0:7} isn't pushed: push it before releasing." >&2
  exit 1
fi

DMG=$(packaging/macos/dmg.sh | tail -n1)

gh release create "$TAG" "$DMG" \
  --target "$COMMIT" \
  --title "Nimbus $VERSION" \
  --generate-notes \
  "$@"
