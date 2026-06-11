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
- 🗂️ **Tree view** by `path` (ordered dev → staging → prod); filter by name, folder, or port.
- ✅ **Multi-select & bulk** Connect / Disconnect / Delete; open a terminal to any tunnel's host.
- 🔑 **In-app SSH configs**; import hosts from `~/.ssh/config` and tunnels from `opentunnels.sh` files.
- 💾 **Portable config** — one YAML file; encrypted export/import via [`age`](https://github.com/FiloSottile/age).
- 🔒 **Local-only** — no account, no telemetry; shells out to your system `ssh` (`~/.ssh/config`, ProxyJump, agent all work).

## Install

**macOS** — one line (downloads the latest [release](https://github.com/japananh/sshoal/releases) `.dmg`, installs to /Applications, clears Gatekeeper):

```sh
curl -fsSL https://raw.githubusercontent.com/japananh/sshoal/main/packaging/macos/install.sh | bash
```

Or grab the `.dmg` from [Releases](https://github.com/japananh/sshoal/releases) and drag **sshoal.app** to Applications. Not notarized yet → on first launch right-click → **Open**.

**From source** (stable [Rust](https://rustup.rs)):

```sh
git clone https://github.com/japananh/sshoal.git && cd sshoal
cargo run -p sshoal      # run the tray app
./scripts/make-dmg.sh    # …or build a .dmg locally
```

Ubuntu also needs `libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libxkbcommon-dev`.

## Use

- **Tray icon** — left-click toggles the window, right-click opens the menu; **⌃⌘S** summons it anywhere. Closing only hides; tunnels keep running.
- **A row** — the toggle connects/disconnects, the terminal icon opens a shell. Click to select, ⌘/Shift-click or ↑/↓ to multi-select, right-click for actions, **Enter** to edit.
- **Config** lives in `~/.config/sshoal/servers.yaml`. Bring in what you have with `sshoal import-ssh --prefix gc FILE…`; move machines with `sshoal export [--encrypt]` / `sshoal import`.

## How it works

A Cargo workspace: **`sshoal-core`** (config, a `Transport` trait over the system `ssh`, and a per-tunnel backoff supervisor — all unit-tested headlessly) and **`app/sshoal`** (an [`iced`](https://github.com/iced-rs/iced) tray daemon).

```sh
cargo test -p sshoal-core    # fast, no network
```

## License

[MIT](LICENSE) © [@japananh](https://github.com/japananh)
