//! sshoal — tray-resident SSH tunnel manager.
//!
//! The app is an `iced::daemon`: it lives in the menu bar / system tray with no
//! window at launch. Opening the window shows the tunnel tree; closing it just
//! hides the window — the daemon (and every tunnel) keeps running until you quit
//! from the tray.
//!
//! Tunnels are organized by their slash-separated `path` into a collapsible
//! tree. Toggling a leaf brings one tunnel up/down; toggling a folder does the
//! same for everything under it. Each tunnel is supervised independently.

mod cli;
mod logging;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{button, column, container, row, scrollable, space, text};
use iced::{Color, Element, Length, Subscription, Task, window};
use sshoal_core::{AppConfig, Backoff, OpenSshTransport, Transport, TunnelState, TunnelSupervisor};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone)]
enum Message {
    /// Periodic: refresh status dots and poll the tray menu channel.
    Tick,
    ToggleTunnel(usize),
    ToggleFolder(String),
    ExpandCollapse(String),
    WindowOpened(window::Id),
}

struct MenuIds {
    connect_all: MenuId,
    open: MenuId,
    quit: MenuId,
}

/// One tunnel plus the supervisor keeping it alive (when enabled).
struct TunnelRow {
    tunnel: sshoal_core::Tunnel,
    supervisor: Option<TunnelSupervisor>,
    status: TunnelState,
}

impl TunnelRow {
    fn enabled(&self) -> bool {
        self.supervisor.is_some()
    }
}

struct App {
    _tray: TrayIcon,
    menu: MenuIds,
    window: Option<window::Id>,
    runtime: Arc<tokio::runtime::Runtime>,
    transport: Arc<dyn Transport>,
    rows: Vec<TunnelRow>,
    /// Folder paths currently expanded in the tree.
    expanded: HashSet<String>,
}

fn boot(runtime: Arc<tokio::runtime::Runtime>) -> (App, Task<Message>) {
    let path = config_path();
    ensure_example(&path);
    let config = AppConfig::load(&path).unwrap_or_else(|err| {
        tracing::error!(error = %err, path = %path.display(), "failed to load config");
        AppConfig::default()
    });
    info!(path = %path.display(), tunnels = config.tunnels.len(), "config loaded");

    let rows: Vec<TunnelRow> = config
        .tunnels
        .into_iter()
        .map(|tunnel| TunnelRow {
            tunnel,
            supervisor: None,
            status: TunnelState::Idle,
        })
        .collect();

    // Expand every folder by default so the whole tree is visible at first.
    let expanded = all_folder_paths(&rows);

    let connect_all = MenuItem::new("Connect all", true, None);
    let open = MenuItem::new("Open sshoal", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = MenuIds {
        connect_all: connect_all.id().clone(),
        open: open.id().clone(),
        quit: quit.id().clone(),
    };
    let tray_menu = Menu::with_items(&[&connect_all, &open, &quit]).expect("build tray menu");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(true)
        .with_icon(make_icon())
        .with_tooltip("sshoal")
        .build()
        .expect("build tray icon");

    let app = App {
        _tray: tray,
        menu,
        window: None,
        runtime,
        transport: Arc::new(OpenSshTransport),
        rows,
        expanded,
    };
    (app, Task::none())
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            for row in &mut app.rows {
                if let Some(sup) = &row.supervisor {
                    row.status = sup.state();
                }
            }

            let rx = MenuEvent::receiver();
            while let Ok(event) = rx.try_recv() {
                if event.id == app.menu.open {
                    if app.window.is_none() {
                        let (id, task) = window::open(window::Settings::default());
                        app.window = Some(id);
                        return task.map(Message::WindowOpened);
                    }
                } else if event.id == app.menu.quit {
                    info!("quitting");
                    return iced::exit();
                } else if event.id == app.menu.connect_all {
                    for i in 0..app.rows.len() {
                        set_enabled(app, i, true);
                    }
                }
            }
            Task::none()
        }
        Message::ToggleTunnel(i) => {
            let on = !app.rows.get(i).map(TunnelRow::enabled).unwrap_or(false);
            set_enabled(app, i, on);
            Task::none()
        }
        Message::ToggleFolder(path) => {
            let indices: Vec<usize> = descendant_indices(&app.rows, &path);
            // If everything under the folder is already on, turn it off; else on.
            let all_on = indices.iter().all(|&i| app.rows[i].enabled());
            for i in indices {
                set_enabled(app, i, !all_on);
            }
            Task::none()
        }
        Message::ExpandCollapse(path) => {
            if !app.expanded.remove(&path) {
                app.expanded.insert(path);
            }
            Task::none()
        }
        Message::WindowOpened(id) => {
            info!(window = ?id, "window opened");
            Task::none()
        }
    }
}

/// Spawn or tear down the supervisor for one tunnel row.
fn set_enabled(app: &mut App, i: usize, on: bool) {
    let transport = app.transport.clone();
    let runtime = app.runtime.clone();
    let Some(row) = app.rows.get_mut(i) else {
        return;
    };

    if on && row.supervisor.is_none() {
        let _guard = runtime.enter();
        let sup = TunnelSupervisor::spawn(transport, row.tunnel.clone(), Backoff::default());
        row.supervisor = Some(sup);
        row.status = TunnelState::Connecting;
        info!(tunnel = %row.tunnel.path, "connect");
    } else if !on && row.supervisor.is_some() {
        if let Some(sup) = row.supervisor.take() {
            sup.cancel();
        }
        row.status = TunnelState::Idle;
        info!(tunnel = %row.tunnel.path, "disconnect");
    }
}

/// Row indices whose tunnel sits at or under `folder_path`.
fn descendant_indices(rows: &[TunnelRow], folder_path: &str) -> Vec<usize> {
    let prefix = format!("{folder_path}/");
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.tunnel.path == folder_path || r.tunnel.path.starts_with(&prefix))
        .map(|(i, _)| i)
        .collect()
}

// ---- tree construction ----

#[derive(Default)]
struct Folder {
    subfolders: BTreeMap<String, Folder>,
    leaves: Vec<usize>,
}

fn build_tree(rows: &[TunnelRow]) -> Folder {
    let mut root = Folder::default();
    for (i, row) in rows.iter().enumerate() {
        let segments = row.tunnel.segments();
        insert_leaf(&mut root, &segments, i);
    }
    root
}

fn insert_leaf(folder: &mut Folder, segments: &[&str], row_idx: usize) {
    match segments {
        [] => {}
        [_leaf] => folder.leaves.push(row_idx),
        [head, rest @ ..] => insert_leaf(
            folder.subfolders.entry(head.to_string()).or_default(),
            rest,
            row_idx,
        ),
    }
}

fn all_folder_paths(rows: &[TunnelRow]) -> HashSet<String> {
    let mut set = HashSet::new();
    for row in rows {
        let segments = row.tunnel.segments();
        // every prefix except the leaf is a folder
        for end in 1..segments.len() {
            set.insert(segments[..end].join("/"));
        }
    }
    set
}

struct DisplayRow {
    depth: usize,
    path: String,
    name: String,
    is_folder: bool,
    enabled: bool,
    status: TunnelState,
}

fn flatten(
    folder: &Folder,
    prefix: &str,
    depth: usize,
    rows: &[TunnelRow],
    expanded: &HashSet<String>,
    out: &mut Vec<DisplayRow>,
) {
    for (name, sub) in &folder.subfolders {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let leaves = collect_leaves(sub);
        let enabled = leaves.iter().any(|&i| rows[i].enabled());
        let status = aggregate(leaves.iter().map(|&i| rows[i].status));
        out.push(DisplayRow {
            depth,
            path: path.clone(),
            name: name.clone(),
            is_folder: true,
            enabled,
            status,
        });
        if expanded.contains(&path) {
            flatten(sub, &path, depth + 1, rows, expanded, out);
        }
    }
    for &idx in &folder.leaves {
        let row = &rows[idx];
        out.push(DisplayRow {
            depth,
            path: row.tunnel.path.clone(),
            name: row.tunnel.name().to_string(),
            is_folder: false,
            enabled: row.enabled(),
            status: row.status,
        });
    }
}

fn collect_leaves(folder: &Folder) -> Vec<usize> {
    let mut out = folder.leaves.clone();
    for sub in folder.subfolders.values() {
        out.extend(collect_leaves(sub));
    }
    out
}

fn aggregate(states: impl Iterator<Item = TunnelState>) -> TunnelState {
    let mut any = false;
    let mut any_up = false;
    let mut any_connecting = false;
    let mut any_bad = false;
    for s in states {
        any = true;
        match s {
            TunnelState::Up => any_up = true,
            TunnelState::Connecting => any_connecting = true,
            TunnelState::Reconnecting | TunnelState::Failed => any_bad = true,
            TunnelState::Idle => {}
        }
    }
    if !any {
        TunnelState::Idle
    } else if any_bad {
        TunnelState::Reconnecting
    } else if any_connecting {
        TunnelState::Connecting
    } else if any_up {
        TunnelState::Up
    } else {
        TunnelState::Idle
    }
}

// ---- view ----

fn view(app: &App, _window: window::Id) -> Element<'_, Message> {
    let mut list = column![].spacing(4);

    if app.rows.is_empty() {
        list = list.push(text(format!(
            "No tunnels yet. Import with `sshoal import-ssh ...` or edit {}.",
            config_path().display()
        )));
    }

    let tree = build_tree(&app.rows);
    let mut display = Vec::new();
    flatten(&tree, "", 0, &app.rows, &app.expanded, &mut display);

    for d in &display {
        let indent = space().width(Length::Fixed(d.depth as f32 * 16.0));

        let lead: Element<Message> = if d.is_folder {
            let arrow = if app.expanded.contains(&d.path) {
                "▾"
            } else {
                "▸"
            };
            button(text(arrow).size(14))
                .padding(2)
                .on_press(Message::ExpandCollapse(d.path.clone()))
                .into()
        } else {
            status_dot(d.status)
        };

        let label = if d.is_folder {
            text(d.name.clone()).size(15)
        } else {
            text(d.name.clone()).size(14)
        }
        .width(Length::Fill);

        let action = button(text(if d.enabled { "Disconnect" } else { "Connect" }).size(12))
            .padding([2, 8])
            .on_press(if d.is_folder {
                Message::ToggleFolder(d.path.clone())
            } else {
                Message::ToggleTunnel(
                    app.rows
                        .iter()
                        .position(|r| r.tunnel.path == d.path)
                        .unwrap_or(0),
                )
            });

        let folder_dot: Element<Message> = if d.is_folder {
            status_dot(d.status)
        } else {
            space().width(Length::Fixed(0.0)).into()
        };

        let line = row![indent, lead, label, folder_dot, action]
            .spacing(8)
            .align_y(iced::Alignment::Center);
        list = list.push(line);
    }

    let header = text("sshoal").size(22);
    let body = scrollable(list).height(Length::Fill);
    container(column![header, body].spacing(12))
        .padding(16)
        .into()
}

fn status_dot(state: TunnelState) -> Element<'static, Message> {
    let color = match state {
        TunnelState::Up => Color::from_rgb(0.18, 0.80, 0.44),
        TunnelState::Connecting => Color::from_rgb(0.95, 0.77, 0.06),
        TunnelState::Reconnecting | TunnelState::Failed => Color::from_rgb(0.90, 0.42, 0.20),
        TunnelState::Idle => Color::from_rgb(0.55, 0.55, 0.60),
    };
    let glyph = if state == TunnelState::Idle {
        "○"
    } else {
        "●"
    };
    text(glyph).size(15).color(color).into()
}

fn subscription(_app: &App) -> Subscription<Message> {
    iced::time::every(Duration::from_millis(200)).map(|_| Message::Tick)
}

/// `~/.config/sshoal/servers.yaml` on both macOS and Linux.
pub(crate) fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/sshoal/servers.yaml")
}

fn ensure_example(path: &Path) {
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(path, EXAMPLE_YAML).is_ok() {
        info!(path = %path.display(), "wrote starter config");
    }
}

const EXAMPLE_YAML: &str = "\
# sshoal config — tunnels to keep alive, organized by `path` (a slash tree).
# `ssh` is an ~/.ssh/config alias (or user@host) passed straight to ssh, so the
# same value works in a plain terminal. Private keys are NOT stored here.
tunnels:
  - path: example/dev/db/app-api
    ssh: gemx-dev
    local_port: 54321
    remote_host: db.internal
    remote_port: 5432
";

fn make_icon() -> Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[0x2e, 0xc4, 0xb6, 0xff]);
    }
    Icon::from_rgba(rgba, w, h).expect("valid rgba icon")
}

fn main() -> iced::Result {
    cli::maybe_run();
    logging::init();

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime"),
    );

    let boot_runtime = runtime.clone();
    iced::daemon(move || boot(boot_runtime.clone()), update, view)
        .subscription(subscription)
        .title(|_app: &App, _id| String::from("sshoal"))
        .run()
}
