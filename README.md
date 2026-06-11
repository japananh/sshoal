<div align="center">

# sshoal

**A menu-bar / system-tray app that keeps your SSH tunnels alive — toggle them with a click, and they auto-reconnect when they drop.**

[![CI](https://github.com/japananh/sshoal/actions/workflows/ci.yml/badge.svg)](https://github.com/japananh/sshoal/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

For macOS (Tahoe+) and Ubuntu (22.04+).

</div>

No more `ssh -N -L …` in a terminal you can't close. sshoal sits in the menu bar,
keeps every tunnel up in the background with live health, and your whole setup is
a plain YAML file you can carry to another machine — no cloud account, no
subscription.

## Features

- 🔌 **Set-and-forget tunnels** — flip a tunnel on and it stays up; a supervisor drives `Idle → Connecting → Up → Reconnecting → Failed` with exponential backoff and reconnects automatically when the link drops.
- 🟢 **Live health** — a status dot per tunnel (and per folder); the failure reason (ssh's own stderr) surfaces inline when something breaks.
- 🗂️ **Organized as a tree** — tunnels are grouped by a slash `path` (`gc/dev/db/app-api`), ordered **dev → staging → prod**, collapsible per folder.
- 🔍 **Instant filter** — search by tunnel name, folder, or local port; filter chips for **All / Connected / Disconnected**.
- ✅ **Multi-select & bulk actions** — ⌘/Shift-click or arrow keys to select, then **Connect all / Disconnect all / Delete** a whole folder or any set of tunnels.
- ⌨️ **Open a terminal** to any tunnel's host in one click (Terminal.app on macOS; the common emulators on Linux).
- 🔑 **GoLand-style SSH configs** — named connection targets (host / user / port / key) managed in-app; tunnels reference one by name.
- 📥 **Imports what you already have** — pull hosts from `~/.ssh/config` and tunnels from `opentunnels.sh`-style files in one command.
- 💾 **Portable, encrypted export** — your config is one YAML file; export/import it (optionally passphrase-encrypted with [`age`](https://github.com/FiloSottile/age)) to move between machines.
- 🔒 **Local-only** — no account, no telemetry, no phone-home. It shells out to your system `ssh`, so `~/.ssh/config`, ProxyJump, agent and `known_hosts` all just work.

## Install

sshoal builds from source today (no packaged installer yet).

```sh
git clone https://github.com/japananh/sshoal.git
cd sshoal

# Run the tray app directly
cargo run -p sshoal

# …or build a proper macOS .app (no Dock icon) into target/release/sshoal.app
./scripts/package-macos.sh
open target/release/sshoal.app

# …or build a drag-to-Applications disk image: target/sshoal-<version>.dmg
./scripts/make-dmg.sh
```

The `.app`/`.dmg` aren't code-signed yet, so on first launch right-click → **Open**
(or `xattr -dr com.apple.quarantine /Applications/sshoal.app`).

Requires the stable Rust toolchain ([rustup](https://rustup.rs)). On Ubuntu you'll
also need the GUI / tray system libraries:

```sh
sudo apt install libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libxkbcommon-dev
```

## Quick start

```sh
# Bring in the tunnels you already keep in opentunnels.sh-style files.
# Hosts (user/port/key) are resolved from ~/.ssh/config automatically.
sshoal import-ssh --prefix gc devservers.sh proddb.sh

# Then launch the app — your tunnels show up in the tray, ready to toggle.
cargo run -p sshoal
```

The tunnel-file format is one forward per line — `localport:remotehost:remoteport  <ssh-alias>  # label`:

```text
54321:db.internal:5432    gemx-dev   # app-api
63799:redis.internal:6379 gemx-dev   # app-api cache
```

No tunnel files? Just hit **＋** in the app to add one by hand.

## Using the app

- **Tray icon** — left-click toggles the popover window; right-click opens the menu (Connect all / Open / Quit). The window can also be summoned with the global hotkey **⌃⌘S** (handy when the icon hides behind the notch). Closing the window only hides it — tunnels keep running until you quit from the tray.
- **A tunnel row** — the toggle on the right connects / disconnects; the terminal icon opens a shell on that host. **Click** a row to select it, **⌘/Shift-click** (or **↑/↓**, **Shift+↑/↓**) to select several, **right-click** for the options menu (Edit / Delete, or Connect all / Disconnect all / Delete for a selection). **Enter** edits the selected tunnel.
- **A folder** — click its icon to expand / collapse; left-click the name to select it; right-click for **Connect all / Disconnect all / Delete**.
- **Esc** backs out one level (popover → menu → form → window).

## Configuration

Everything lives in a single file at `~/.config/sshoal/servers.yaml` — two lists,
**ssh configs** (named connection targets) and **tunnels** (each placed in the tree
by `path` and pointing at an ssh config by name):

```yaml
ssh_configs:
  - name: dev
    host: 1.2.3.4
    user: deploy
    identity_file: ~/.ssh/dev.pem
tunnels:
  - path: gc/dev/db/app-api
    ssh: dev
    local_port: 54321
    remote_host: db.internal
    remote_port: 5432
```

Private keys are never stored — only a path to the key file.

### Move it to another machine

```sh
sshoal export backup.yaml              # plain YAML (paths to keys, no secrets)
sshoal export backup.age --encrypt     # passphrase-encrypted (age)
sshoal import backup.age               # merge into this machine's config
```

The passphrase is read from `$SSHOAL_PASSPHRASE`, otherwise prompted.

## How it works

A small Cargo workspace, with all the logic separated from the UI so it stays
testable and portable:

- **`crates/sshoal-core`** — UI-free, unit-tested headlessly:
  - `config` — the `servers.yaml` model (load / save / merge; the export/import unit).
  - `transport` — a `Transport` trait; `OpenSshTransport` shells out to the system `ssh` (`-N -L …`), captures its stderr for failure reasons, and confirms a tunnel is **Up** by connecting to the local port.
  - `supervisor` — one Tokio task per tunnel running the state machine with backoff (and a `stable_after` window so a flapping link doesn't hot-loop).
  - `transfer` — export / import with optional `age` encryption.
- **`app/sshoal`** — the binary: an [`iced`](https://github.com/iced-rs/iced) tray-resident daemon. It renders the tree, drives a supervisor per tunnel, and cleans up orphaned `ssh` processes from a previous run on launch.

## Develop

```sh
cargo test -p sshoal-core              # fast, no network
cargo run -p sshoal                    # launch the tray app
cargo clippy --all-targets -- -D warnings
```

## License

[MIT](LICENSE) © [@japananh](https://github.com/japananh)
