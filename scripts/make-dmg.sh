#!/usr/bin/env bash
#
# Build a distributable sshoal.dmg — a drag-to-Applications disk image.
#
# Builds sshoal.app (via package-macos.sh) and packs it into a compressed .dmg
# alongside an /Applications alias, so installing is just "drag the app onto the
# Applications folder". The app is NOT code-signed/notarized, so on first launch
# macOS Gatekeeper will block it — right-click → Open, or run:
#   xattr -dr com.apple.quarantine /Applications/sshoal.app
set -euo pipefail

cd "$(dirname "$0")/.."

# 1. Build the .app bundle.
./scripts/package-macos.sh

APP="target/release/sshoal.app"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
DMG="target/sshoal-${VERSION}.dmg"

# 2. Stage the bundle + an Applications alias for drag-to-install.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

# 3. Build a compressed read-only image.
echo "==> building $DMG"
rm -f "$DMG"
hdiutil create \
    -volname "sshoal" \
    -srcfolder "$STAGE" \
    -fs HFS+ \
    -format UDZO \
    -ov \
    "$DMG" >/dev/null

echo "==> done: $DMG"
echo "    open it, drag sshoal.app onto Applications, then on first launch:"
echo "    right-click → Open  (or: xattr -dr com.apple.quarantine /Applications/sshoal.app)"
