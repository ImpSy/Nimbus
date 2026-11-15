#!/usr/bin/env bash
# Linux / Steam Deck release build (runs inside the build container, or on a
# Linux machine with the -dev packages listed in the Dockerfile).
# Output: target/deck/nimbus and target/deck/nimbus-linux-<arch>.tar.gz
set -euo pipefail
cd "$(dirname "$0")/../.."
mise install
mise run build
BIN="${CARGO_TARGET_DIR:-target}/release/nimbus"
file "$BIN"
echo "Dynamic dependencies:"
ldd "$BIN" | sed 's/^/  /'

ARCH=$(uname -m)
OUT=target/deck
APPDIR="$OUT/Nimbus.AppDir"
rm -rf "$OUT"
mkdir -p "$APPDIR/usr/bin"
cp "$BIN" "$OUT/nimbus"
cp "$BIN" "$APPDIR/usr/bin/nimbus"
cp packaging/linux/nimbus.desktop packaging/linux/nimbus.png "$APPDIR/"
cat > "$APPDIR/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/nimbus" "$@"
RUN
chmod +x "$APPDIR/AppRun"
if command -v appimagetool >/dev/null; then
  ARCH=$ARCH appimagetool "$APPDIR" "$OUT/Nimbus-$ARCH.AppImage"
  echo "AppImage: $OUT/Nimbus-$ARCH.AppImage"
fi
tar -C "$OUT" -czf "$OUT/nimbus-linux-$ARCH.tar.gz" nimbus Nimbus.AppDir
echo "Built $OUT/nimbus and $OUT/nimbus-linux-$ARCH.tar.gz"
