<div align="center">

# sshoal

**Keep your SSH tunnels alive from the menu bar — one click to toggle, auto-reconnect when they drop.**

[![CI](https://github.com/japananh/sshoal/actions/workflows/ci.yml/badge.svg)](https://github.com/japananh/sshoal/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/japananh/sshoal?include_prereleases&sort=semver)](https://github.com/japananh/sshoal/releases)

macOS (Tahoe+) · Ubuntu (22.04+)

</div>

## Features

- 🔌 **Auto-reconnecting tunnels** with live health — a backoff supervisor keeps each one up and surfaces failures inline.
- 🚀 **Open at login & resume** — optionally relaunch sshoal when you log in and reconnect the tunnels that were up when you last quit (Preferences → General), so your session is back after a reboot.
- 🗂️ **Tree view** by `path` (ordered dev → staging → prod); filter by name, folder, or port.
- ✅ **Multi-select & bulk** Connect / Disconnect / Delete; open a terminal to any tunnel's host.
- 🔑 **In-app SSH configs**; import hosts from `~/.ssh/config` and tunnels from `opentunnels.sh` files.
- 💾 **Portable config** — one YAML file; passphrase-encrypted export/import (Argon2id + XChaCha20-Poly1305), optionally bundling the referenced private keys.
- 🔄 **Update check** — pings GitHub Releases on launch (opt-out in Preferences) and shows a banner; never auto-installs.
- 🔒 **Local-only** — no account, no telemetry; shells out to your system `ssh` (`~/.ssh/config`, ProxyJump, agent all work).

## Install

**macOS** — one line (downloads the latest [release](https://github.com/japananh/sshoal/releases) `.dmg`, installs to /Applications, clears Gatekeeper, and links the `sshoal` command onto your PATH):

```sh
curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/macos/install.sh | bash
```

Or grab the `.dmg` from [Releases](https://github.com/japananh/sshoal/releases) and drag **sshoal.app** to Applications. Not notarized yet → on first launch right-click → **Open**. A manual drag skips the CLI link; add it with `ln -sf /Applications/sshoal.app/Contents/MacOS/sshoal /usr/local/bin/sshoal`.

**Linux** (x86_64) — drops the `sshoal` binary into `~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/linux/install.sh | sh
```

Needs a system tray (on GNOME, the AppIndicator extension) and the GTK/AppIndicator runtime libraries: `libgtk-3-0 libayatana-appindicator3-1 libxdo3 libxkbcommon0`.

**From source** (stable [Rust](https://rustup.rs)):

```sh
git clone https://github.com/japananh/sshoal.git && cd sshoal
cargo run -p sshoal      # run the tray app
./scripts/make-dmg.sh    # …or build a .dmg locally
```

Building on Ubuntu also needs the `-dev` packages: `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libxkbcommon-dev`.

## Use

- **Tray icon** — left-click toggles the window, right-click opens the menu; **⌃⌘S** summons it anywhere. Closing only hides; tunnels keep running.
- **A row** — the toggle connects/disconnects, the terminal icon opens a shell. Click to select; ⌘/Shift-click (or ↑/↓, Shift+↑/↓) to select more; right-click for actions; **Enter** to edit.
- **From the CLI** — drive the running app without opening it: `sshoal list`, `sshoal connect PATH`, `sshoal disconnect PATH`, `sshoal status PATH`. `PATH` is a full tunnel path or a folder (connects everything under it). `connect` waits until each tunnel is up and exits non-zero if any didn't (so scripts and agents can gate on it); `--no-wait` to fire-and-forget. Needs the app running (over a private per-user socket at `~/.config/sshoal/control.sock`).
- **Config** lives in `~/.config/sshoal/servers.yaml`. Bring in what you have with `sshoal import-ssh --prefix gc FILE…`; back up or move a machine with `sshoal export` / `sshoal import` — or **Preferences → Backup** in the app. Exports are encrypted by default (`--plaintext` to skip); `--include-keys` also bundles the referenced private keys and restores them on import. A backup also carries your open-at-login / resume preferences (and which tunnels were connected), so a restore brings your setup back.

## How it works

A Cargo workspace: **`sshoal-core`** (config, a `Transport` trait over the system `ssh`, and a per-tunnel backoff supervisor — all unit-tested headlessly) and **`app/sshoal`** (an [`iced`](https://github.com/iced-rs/iced) tray daemon).

```sh
cargo test -p sshoal-core    # fast, no network
```

## License

[MIT](LICENSE) © [@japananh](https://github.com/japananh)
