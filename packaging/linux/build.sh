#!/usr/bin/env bash
# Build a release binary (+ AppImage when appimagetool is available).
set -euo pipefail
cd "$(dirname "$0")/../.."
cargo build --release -p nimbus --features static-ffmpeg-linux
BIN=target/release/nimbus
file "$BIN"
echo "Dynamic dependencies:"
ldd "$BIN" | sed 's/^/  /'

APPDIR=target/Nimbus.AppDir
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"
cp "$BIN" "$APPDIR/usr/bin/nimbus"
cp packaging/linux/nimbus.desktop "$APPDIR/"
cp packaging/linux/nimbus.png "$APPDIR/" 2>/dev/null || true
cat > "$APPDIR/AppRun" <<'RUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/nimbus" "$@"
RUN
chmod +x "$APPDIR/AppRun"
if command -v appimagetool >/dev/null; then
  ARCH=x86_64 appimagetool "$APPDIR" target/Nimbus-x86_64.AppImage
  echo "AppImage: target/Nimbus-x86_64.AppImage"
else
  tar -C target -czf target/nimbus-linux-x86_64.tar.gz Nimbus.AppDir
  echo "Tarball: target/nimbus-linux-x86_64.tar.gz (appimagetool not installed)"
fi
