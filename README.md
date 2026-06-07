# sshoal

A lightweight menu-bar / system-tray app to toggle and **keep multiple SSH
tunnels alive** — they auto-reconnect when they drop, so you don't have to open
a terminal and re-run `ssh` by hand. Built for macOS (Tahoe+) and Ubuntu (22+).

> Status: early. The core (config, SSH transport, reconnect/backoff supervisor)
> is in place and tested; the tray UI is being wired up.

## Why

A "set and forget" tunnel manager: turn a server on, its tunnels stay up in the
background with health status, and your config is a plain file you can carry to
another machine — no cloud account, no subscription.

## Architecture

A Cargo workspace:

- **`crates/sshoal-core`** — all the logic, UI-free and unit-tested headlessly:
  - `config` — the `servers.yaml` model (load/save; the export/import unit).
  - `transport` — the `Transport` trait; `OpenSshTransport` shells out to the
    system `ssh` so `~/.ssh/config`, ProxyJump, agent and known_hosts all work.
  - `supervisor` — one Tokio task per tunnel driving
    `Idle → Connecting → Up → Reconnecting → Failed` with exponential backoff.
- **`app/sshoal`** — the binary: an `iced` tray-resident daemon. Closing the
  window hides it; tunnels keep running; quit from the tray menu.

## Develop

```bash
cargo test -p sshoal-core   # fast, no network
cargo run -p sshoal         # launch the tray app
```

## License

MIT
