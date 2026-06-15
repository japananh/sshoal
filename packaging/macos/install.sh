#!/bin/sh
# Install the latest sshoal release into /Applications.
#
#   curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/macos/install.sh | bash
#
# Downloads the newest release .dmg (including pre-releases), copies sshoal.app
# to /Applications, and clears the Gatekeeper quarantine flag (the app isn't
# notarized yet).
set -eu

REPO="japananh/sshoal"

echo "==> finding the latest sshoal release"
DMG_URL=$(
    curl -fsSL "https://api.github.com/repos/$REPO/releases" |
        grep -o 'https://[^"]*\.dmg' | head -1
)
if [ -z "${DMG_URL:-}" ]; then
    echo "error: no .dmg asset found in $REPO releases" >&2
    exit 1
fi

TMP=$(mktemp -d)
MNT="$TMP/mnt"
trap 'hdiutil detach "$MNT" -quiet 2>/dev/null || true; rm -rf "$TMP"' EXIT

echo "==> downloading $(basename "$DMG_URL")"
curl -fsSL "$DMG_URL" -o "$TMP/sshoal.dmg"

echo "==> installing to /Applications"
# Mount at our own path so we never parse hdiutil output (-quiet hides it).
mkdir -p "$MNT"
hdiutil attach "$TMP/sshoal.dmg" -nobrowse -quiet -mountpoint "$MNT"
rm -rf /Applications/sshoal.app
cp -R "$MNT/sshoal.app" /Applications/
hdiutil detach "$MNT" -quiet
xattr -dr com.apple.quarantine /Applications/sshoal.app 2>/dev/null || true

echo "==> done. Launch from Spotlight, or:  open -a sshoal"
