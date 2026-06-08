//! sshoal — tray-resident SSH tunnel manager.
//!
//! The app is an `iced::daemon`: it lives in the menu bar / system tray with no
//! window at launch. Opening the window shows the server list; closing it just
//! hides the window — the daemon (and every tunnel) keeps running until you quit
//! from the tray.
//!
//! All the real work lives in `sshoal-core`. This binary is the thin shell:
//! load config, spawn a [`TunnelSupervisor`] per tunnel on a Tokio runtime, and
//! render their live state.

mod logging;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Color, Element, Length, Subscription, Task, window};
use sshoal_core::{
    AppConfig, Backoff, OpenSshTransport, ServerConfig, Transport, TunnelState, TunnelSupervisor,
};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone)]
enum Message {
    /// Periodic: refresh status dots and poll the tray menu channel.
    Tick,
    /// Toggle a server's tunnels on/off (from the window button).
    Toggle(usize),
    WindowOpened(window::Id),
}

/// Tray menu item ids we match clicks against.
struct MenuIds {
    connect_all: MenuId,
    open: MenuId,
    quit: MenuId,
}

/// One row in the server list: its config, whether it's on, the supervisors
/// keeping its tunnels alive, and the aggregated status shown as a dot.
struct ServerRow {
    config: ServerConfig,
    enabled: bool,
    supervisors: Vec<TunnelSupervisor>,
    status: TunnelState,
}

struct App {
    /// Kept alive for the whole process — dropping it removes the tray icon.
    _tray: TrayIcon,
    menu: MenuIds,
    window: Option<window::Id>,
    /// Runtime that hosts the tunnel supervisor tasks.
    runtime: Arc<tokio::runtime::Runtime>,
    transport: Arc<dyn Transport>,
    servers: Vec<ServerRow>,
}

fn boot(runtime: Arc<tokio::runtime::Runtime>) -> (App, Task<Message>) {
    let path = config_path();
    ensure_example(&path);
    let config = AppConfig::load(&path).unwrap_or_else(|err| {
        tracing::error!(error = %err, path = %path.display(), "failed to load config");
        AppConfig::default()
    });
    info!(path = %path.display(), servers = config.servers.len(), "config loaded");

    let servers = config
        .servers
        .into_iter()
        .map(|config| ServerRow {
            config,
            enabled: false,
            supervisors: Vec::new(),
            status: TunnelState::Idle,
        })
        .collect();

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
        servers,
    };
    (app, Task::none())
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            for row in &mut app.servers {
                if row.enabled && !row.supervisors.is_empty() {
                    row.status = aggregate(&row.supervisors);
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
                    for i in 0..app.servers.len() {
                        set_enabled(app, i, true);
                    }
                }
            }
            Task::none()
        }
        Message::Toggle(i) => {
            let on = !app.servers.get(i).map(|r| r.enabled).unwrap_or(false);
            set_enabled(app, i, on);
            Task::none()
        }
        Message::WindowOpened(id) => {
            info!(window = ?id, "window opened");
            Task::none()
        }
    }
}

/// Spawn or tear down a server's tunnel supervisors.
fn set_enabled(app: &mut App, i: usize, on: bool) {
    let transport = app.transport.clone();
    let runtime = app.runtime.clone();
    let Some(row) = app.servers.get_mut(i) else {
        return;
    };

    if on && !row.enabled {
        let _guard = runtime.enter();
        row.supervisors = row
            .config
            .tunnels
            .iter()
            .map(|tunnel| {
                TunnelSupervisor::spawn(
                    transport.clone(),
                    row.config.clone(),
                    tunnel.clone(),
                    Backoff::default(),
                )
            })
            .collect();
        row.enabled = true;
        row.status = if row.supervisors.is_empty() {
            TunnelState::Idle
        } else {
            TunnelState::Connecting
        };
        info!(server = %row.config.name, tunnels = row.supervisors.len(), "connect");
    } else if !on && row.enabled {
        for supervisor in row.supervisors.drain(..) {
            supervisor.cancel();
        }
        row.enabled = false;
        row.status = TunnelState::Idle;
        info!(server = %row.config.name, "disconnect");
    }
}

/// Combine per-tunnel states into one status for the server row.
fn aggregate(supervisors: &[TunnelSupervisor]) -> TunnelState {
    let mut all_up = true;
    let mut any_connecting = false;
    let mut any_bad = false;
    for supervisor in supervisors {
        match supervisor.state() {
            TunnelState::Up => {}
            TunnelState::Connecting => {
                all_up = false;
                any_connecting = true;
            }
            TunnelState::Reconnecting | TunnelState::Failed => {
                all_up = false;
                any_bad = true;
            }
            TunnelState::Idle => all_up = false,
        }
    }
    if all_up {
        TunnelState::Up
    } else if any_bad {
        TunnelState::Reconnecting
    } else if any_connecting {
        TunnelState::Connecting
    } else {
        TunnelState::Idle
    }
}

fn view(app: &App, _window: window::Id) -> Element<'_, Message> {
    let mut list = column![].spacing(8);

    if app.servers.is_empty() {
        list = list.push(text(format!(
            "No servers yet. Edit {} and reopen.",
            config_path().display()
        )));
    }

    for (i, row) in app.servers.iter().enumerate() {
        let target = match &row.config.user {
            Some(user) => format!("{user}@{}:{}", row.config.host, row.config.port),
            None => format!("{}:{}", row.config.host, row.config.port),
        };
        let label = column![
            text(row.config.name.clone()).size(16),
            text(target)
                .size(12)
                .color(Color::from_rgb(0.55, 0.55, 0.6)),
        ]
        .spacing(2)
        .width(Length::Fill);

        let action = button(text(if row.enabled { "Disconnect" } else { "Connect" }))
            .on_press(Message::Toggle(i));

        let line = row![status_dot(row.status), label, action]
            .spacing(12)
            .align_y(iced::Alignment::Center);
        list = list.push(container(line).padding(8));
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
    let glyph = if state == TunnelState::Idle { "○" } else { "●" };
    text(glyph).size(16).color(color).into()
}

fn subscription(_app: &App) -> Subscription<Message> {
    iced::time::every(Duration::from_millis(200)).map(|_| Message::Tick)
}

/// `~/.config/sshoal/servers.yaml` on both macOS and Linux.
fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/sshoal/servers.yaml")
}

/// Write a starter config the first time the app runs, so there's something to edit.
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
# sshoal config — the servers and tunnels to keep alive.
# Private keys are NOT stored here; sshoal uses your existing ~/.ssh setup
# (config, keys, agent, known_hosts). `host` may be an alias from ~/.ssh/config.
servers:
  - name: Example DB
    host: example.com
    port: 22
    user: deploy
    group: staging
    tunnels:
      - local_port: 5432
        remote_host: 127.0.0.1
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
