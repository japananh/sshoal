#!/bin/sh
# Install the latest sshoal release into /Applications.
#
#   curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/macos/install.sh | bash
#
# Downloads the newest release .dmg (including pre-releases), copies sshoal.app
# to /Applications, clears the Gatekeeper quarantine flag (the app isn't
# notarized yet), and links the `sshoal` CLI onto your PATH.
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

# Link the CLI onto the PATH so `sshoal export` / `import` work in a terminal —
# the same binary is both the tray app and the CLI. Prefer a dir we can write
# without sudo; fall back to /usr/local/bin (on the default PATH) with sudo.
echo "==> linking the sshoal CLI onto your PATH"
BIN=/Applications/sshoal.app/Contents/MacOS/sshoal
LINK=""
for d in /opt/homebrew/bin /usr/local/bin; do
    if [ -d "$d" ] && [ -w "$d" ]; then
        ln -sf "$BIN" "$d/sshoal" && LINK="$d/sshoal" && break
    fi
done
if [ -z "$LINK" ] && sudo mkdir -p /usr/local/bin && sudo ln -sf "$BIN" /usr/local/bin/sshoal; then
    LINK=/usr/local/bin/sshoal
fi
if [ -n "$LINK" ]; then
    echo "    linked $LINK"
else
    echo "    (couldn't link automatically — run: sudo ln -sf '$BIN' /usr/local/bin/sshoal)"
fi

echo "==> done. Launch from Spotlight or 'open -a sshoal'; CLI: 'sshoal help'"
