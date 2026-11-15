#!/usr/bin/env bash
# Build a self-contained target/Nimbus.app (SDL3 linked statically; video via VideoToolbox).
# Usage: mise run bundle:macos
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build --release -p nimbus

APP=target/Nimbus.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/nimbus "$APP/Contents/MacOS/nimbus"
sips -s format icns packaging/icons/nimbus-512.png --out "$APP/Contents/Resources/nimbus.icns" >/dev/null
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Nimbus</string>
  <key>CFBundleDisplayName</key><string>Nimbus</string>
  <key>CFBundleIdentifier</key><string>dev.nimbus.Nimbus</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>nimbus</string>
  <key>CFBundleIconFile</key><string>nimbus</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.games</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>GCSupportsGameMode</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><false/>
</dict>
</plist>
PLIST
# Ad-hoc sign so Gatekeeper lets it run locally.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true
echo "Built $APP"
otool -L "$APP/Contents/MacOS/nimbus" | sed 's/^/  /'
