#!/bin/sh
# Install the latest sshoal release on Linux (x86_64).
#
#   curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/linux/install.sh | sh
#
# Downloads the newest release tarball (including pre-releases) and drops the
# `sshoal` binary into ~/.local/bin (override with PREFIX=/usr/local/bin, which
# may need sudo). sshoal is a GTK/AppIndicator tray app — your desktop needs a
# system tray; on GNOME install the AppIndicator extension.
set -eu

REPO="japananh/sshoal"
BIN_DIR="${PREFIX:-$HOME/.local/bin}"

echo "==> finding the latest sshoal release"
URL=$(
    curl -fsSL "https://api.github.com/repos/$REPO/releases" |
        grep -o 'https://[^"]*linux-x86_64\.tar\.gz' | head -1
)
if [ -z "${URL:-}" ]; then
    echo "error: no linux-x86_64 asset found in $REPO releases" >&2
    exit 1
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "==> downloading $(basename "$URL")"
curl -fsSL "$URL" -o "$TMP/sshoal.tar.gz"
tar -C "$TMP" -xzf "$TMP/sshoal.tar.gz"

echo "==> installing to $BIN_DIR/sshoal"
mkdir -p "$BIN_DIR"
install -m 0755 "$TMP/sshoal" "$BIN_DIR/sshoal"

echo "==> done. run:  sshoal"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "    (add $BIN_DIR to your PATH first)" ;;
esac
