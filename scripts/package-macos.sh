#!/usr/bin/env bash
#
# Build sshoal.app — a menu-bar-only macOS bundle.
#
# The key bit is `LSUIElement` in Info.plist: it makes sshoal an "agent" app, so
# it lives only in the menu bar with no Dock icon and no app menu, which is what
# a tray utility wants. (Running the bare `cargo run` binary still shows a Dock
# icon — only the bundled .app is agent-mode.)
set -euo pipefail

cd "$(dirname "$0")/.."

APP="target/release/sshoal.app"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
# CFBundleShortVersionString must be numeric x.y.z — drop any -beta/-rc suffix.
SHORT_VERSION="${VERSION%%-*}"

echo "==> building release binary"
cargo build --release -p sshoal

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/sshoal "$APP/Contents/MacOS/sshoal"

echo "==> rendering icon"
ICONSET="target/sshoal.iconset"
rm -rf "$ICONSET"
cargo run --release --example gen_icon -p sshoal -- "$ICONSET" >/dev/null
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>sshoal</string>
    <key>CFBundleDisplayName</key>
    <string>sshoal</string>
    <key>CFBundleIdentifier</key>
    <string>dev.japananh.sshoal</string>
    <key>CFBundleExecutable</key>
    <string>sshoal</string>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${SHORT_VERSION}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

echo "==> done: $APP"
echo "    run with:  open $APP"
