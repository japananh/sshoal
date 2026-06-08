//! sshoal — tray-resident SSH tunnel manager.
//!
//! The app is an `iced::daemon`: it lives in the menu bar / system tray with no
//! window at launch. Opening the window shows the tunnel tree; closing it just
//! hides the window — the daemon (and every tunnel) keeps running until you quit
//! from the tray.
//!
//! Tunnels are organized by their slash-separated `path` into a collapsible
//! tree. Toggling a leaf brings one tunnel up/down; toggling a folder does the
//! same for everything under it. Tunnels can be added/edited/deleted in-app;
//! changes are written back to the config file.

mod cli;
mod logging;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{button, column, container, row, scrollable, space, text, text_input, toggler};
use iced::{Color, Element, Length, Size, Subscription, Task, Theme, window};
use sshoal_core::{
    AppConfig, Backoff, OpenSshTransport, Transport, Tunnel, TunnelState, TunnelSupervisor,
};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, Copy)]
enum Field {
    Path,
    Ssh,
    SshPort,
    LocalPort,
    RemoteHost,
    RemotePort,
}

#[derive(Debug, Clone)]
enum Message {
    /// Periodic: refresh status dots and poll the tray menu channel.
    Tick,
    ToggleTunnel(usize),
    ToggleFolder(String),
    ExpandCollapse(String),
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    StartAdd,
    StartEdit(usize),
    EditField(Field, String),
    SaveEdit,
    CancelEdit,
    DeleteTunnel(usize),
}

struct MenuIds {
    connect_all: MenuId,
    open: MenuId,
    quit: MenuId,
}

/// One tunnel plus the supervisor keeping it alive (when enabled).
struct TunnelRow {
    tunnel: Tunnel,
    supervisor: Option<TunnelSupervisor>,
    status: TunnelState,
}

impl TunnelRow {
    fn enabled(&self) -> bool {
        self.supervisor.is_some()
    }
}

/// In-progress add/edit form. Ports are kept as strings while typing.
#[derive(Default)]
struct EditForm {
    target: Option<usize>, // Some(idx) = editing existing, None = adding new
    path: String,
    ssh: String,
    ssh_port: String,
    local_port: String,
    remote_host: String,
    remote_port: String,
    error: Option<String>,
}

struct App {
    _tray: TrayIcon,
    menu: MenuIds,
    window: Option<window::Id>,
    runtime: Arc<tokio::runtime::Runtime>,
    transport: Arc<dyn Transport>,
    rows: Vec<TunnelRow>,
    expanded: HashSet<String>,
    editing: Option<EditForm>,
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
        editing: None,
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
                        let settings = window::Settings {
                            size: Size::new(420.0, 620.0),
                            min_size: Some(Size::new(340.0, 380.0)),
                            ..window::Settings::default()
                        };
                        let (id, task) = window::open(settings);
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
            let indices = descendant_indices(&app.rows, &path);
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
        Message::StartAdd => {
            app.editing = Some(EditForm {
                ssh: "gemx-dev".to_string(),
                ..EditForm::default()
            });
            Task::none()
        }
        Message::StartEdit(i) => {
            if let Some(row) = app.rows.get(i) {
                let t = &row.tunnel;
                app.editing = Some(EditForm {
                    target: Some(i),
                    path: t.path.clone(),
                    ssh: t.ssh.clone(),
                    ssh_port: t.ssh_port.map(|p| p.to_string()).unwrap_or_default(),
                    local_port: t.local_port.to_string(),
                    remote_host: t.remote_host.clone(),
                    remote_port: t.remote_port.to_string(),
                    error: None,
                });
            }
            Task::none()
        }
        Message::EditField(field, value) => {
            if let Some(form) = &mut app.editing {
                match field {
                    Field::Path => form.path = value,
                    Field::Ssh => form.ssh = value,
                    Field::SshPort => form.ssh_port = value,
                    Field::LocalPort => form.local_port = value,
                    Field::RemoteHost => form.remote_host = value,
                    Field::RemotePort => form.remote_port = value,
                }
            }
            Task::none()
        }
        Message::CancelEdit => {
            app.editing = None;
            Task::none()
        }
        Message::SaveEdit => {
            save_edit(app);
            Task::none()
        }
        Message::DeleteTunnel(i) => {
            app.editing = None;
            if i < app.rows.len() {
                let mut row = app.rows.remove(i);
                if let Some(sup) = row.supervisor.take() {
                    sup.cancel();
                }
                info!(tunnel = %row.tunnel.path, "delete");
                app.expanded = all_folder_paths(&app.rows);
                persist(&app.rows);
            }
            Task::none()
        }
        Message::WindowOpened(id) => {
            info!(window = ?id, "window opened");
            Task::none()
        }
        Message::WindowClosed(id) => {
            // Forget the window so the next "Open" can spawn a fresh one.
            if app.window == Some(id) {
                app.window = None;
            }
            Task::none()
        }
    }
}

/// Validate the edit form and apply it (replace or append a tunnel), then save.
fn save_edit(app: &mut App) {
    let Some(form) = &app.editing else { return };

    let parse_port = |s: &str| s.trim().parse::<u16>().ok();
    let path = form.path.trim().to_string();
    let ssh = form.ssh.trim().to_string();
    let remote_host = form.remote_host.trim().to_string();

    let error = if path.is_empty() {
        Some("Path is required")
    } else if ssh.is_empty() {
        Some("SSH target is required")
    } else if remote_host.is_empty() {
        Some("Remote host is required")
    } else if parse_port(&form.local_port).is_none() {
        Some("Local port must be 1–65535")
    } else if parse_port(&form.remote_port).is_none() {
        Some("Remote port must be 1–65535")
    } else if !form.ssh_port.trim().is_empty() && parse_port(&form.ssh_port).is_none() {
        Some("SSH port must be 1–65535")
    } else {
        None
    };

    if let Some(msg) = error {
        if let Some(form) = &mut app.editing {
            form.error = Some(msg.to_string());
        }
        return;
    }

    let tunnel = Tunnel {
        path,
        ssh,
        ssh_port: parse_port(&form.ssh_port),
        local_port: parse_port(&form.local_port).unwrap(),
        remote_host,
        remote_port: parse_port(&form.remote_port).unwrap(),
    };

    match form.target {
        Some(i) if i < app.rows.len() => {
            // Editing: drop any live tunnel so it reconnects with new settings.
            if let Some(sup) = app.rows[i].supervisor.take() {
                sup.cancel();
            }
            app.rows[i].status = TunnelState::Idle;
            app.rows[i].tunnel = tunnel;
        }
        _ => app.rows.push(TunnelRow {
            tunnel,
            supervisor: None,
            status: TunnelState::Idle,
        }),
    }

    app.editing = None;
    app.expanded = all_folder_paths(&app.rows);
    persist(&app.rows);
}

/// Write the current tunnels back to the config file.
fn persist(rows: &[TunnelRow]) {
    let config = AppConfig {
        tunnels: rows.iter().map(|r| r.tunnel.clone()).collect(),
    };
    if let Err(e) = config.save(config_path()) {
        tracing::error!(error = %e, "failed to save config");
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
        insert_leaf(&mut root, &row.tunnel.segments(), i);
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
    row_idx: Option<usize>, // Some for leaves
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
            row_idx: None,
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
            row_idx: Some(idx),
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
    if let Some(form) = &app.editing {
        return edit_view(form);
    }

    let header = row![
        text("sshoal").size(22).width(Length::Fill),
        button(text("+ Add").size(13))
            .style(pill_button)
            .padding([5, 14])
            .on_press(Message::StartAdd),
    ]
    .align_y(iced::Alignment::Center);

    let mut list = column![].spacing(2);
    if app.rows.is_empty() {
        list = list.push(text("No tunnels yet. Click + Add, or run `sshoal import-ssh`.").size(13));
    }

    let tree = build_tree(&app.rows);
    let mut display = Vec::new();
    flatten(&tree, "", 0, &app.rows, &app.expanded, &mut display);

    for d in &display {
        list = list.push(tree_row(app, d));
    }

    let body = scrollable(list).height(Length::Fill);
    container(column![header, body].spacing(12))
        .padding(14)
        .into()
}

fn tree_row<'a>(app: &App, d: &DisplayRow) -> Element<'a, Message> {
    let indent = space().width(Length::Fixed(d.depth as f32 * 16.0));
    let is_folder = d.row_idx.is_none();

    let lead: Element<Message> = if is_folder {
        let icon = if app.expanded.contains(&d.path) {
            "📂"
        } else {
            "📁"
        };
        button(text(icon).size(15))
            .style(button::text)
            .padding(2)
            .on_press(Message::ExpandCollapse(d.path.clone()))
            .into()
    } else {
        status_dot(d.status)
    };

    // Name: folders just show text; leaves are a text-styled button → edit.
    let name: Element<Message> = if is_folder {
        text(d.name.clone()).size(14).width(Length::Fill).into()
    } else {
        button(text(d.name.clone()).size(13))
            .style(button::text)
            .padding(0)
            .width(Length::Fill)
            .on_press(Message::StartEdit(d.row_idx.unwrap()))
            .into()
    };

    // On/off switch: cleaner than a text button, and doesn't read as up/down.
    let switch: Element<Message> = if is_folder {
        let path = d.path.clone();
        toggler(d.enabled)
            .size(18)
            .on_toggle(move |_| Message::ToggleFolder(path.clone()))
            .into()
    } else {
        let idx = d.row_idx.unwrap();
        toggler(d.enabled)
            .size(18)
            .on_toggle(move |_| Message::ToggleTunnel(idx))
            .into()
    };

    let mut line = row![indent, lead, name].spacing(10);
    if is_folder {
        line = line.push(status_dot(d.status));
    }
    line = line.push(switch);
    // Keep controls clear of the scrollbar on the right.
    line = line.push(space().width(Length::Fixed(8.0)));

    let line = line.align_y(iced::Alignment::Center);
    if is_folder {
        // Highlight folder rows with a subtle background band.
        container(line)
            .width(Length::Fill)
            .padding([5, 6])
            .style(folder_band)
            .into()
    } else {
        container(line).padding([3, 6]).into()
    }
}

/// Soft, light, rounded band behind folder rows (macOS-list feel).
fn folder_band(_theme: &iced::Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.945, 0.945, 0.96))),
        border: iced::Border {
            radius: 8.0.into(),
            ..Default::default()
        },
        ..iced::widget::container::Style::default()
    }
}

/// Rounded "pill" button for the Add action.
fn pill_button(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let base = button::primary(theme, status);
    iced::widget::button::Style {
        border: iced::Border {
            radius: 14.0.into(),
            ..base.border
        },
        ..base
    }
}

fn edit_view(form: &EditForm) -> Element<'_, Message> {
    let title = if form.target.is_some() {
        "Edit tunnel"
    } else {
        "New tunnel"
    };

    let field = |label: &str, value: &str, f: Field, placeholder: &str| -> Element<Message> {
        row![
            text(label.to_string()).size(13).width(Length::Fixed(110.0)),
            text_input(placeholder, value)
                .size(13)
                .on_input(move |s| Message::EditField(f, s)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
    };

    let mut col = column![
        text(title).size(18),
        field("Path", &form.path, Field::Path, "gc/dev/db/app-api"),
        field("SSH", &form.ssh, Field::Ssh, "gemx-dev or user@host"),
        field("SSH port", &form.ssh_port, Field::SshPort, "(optional)"),
        field("Local port", &form.local_port, Field::LocalPort, "54321"),
        field(
            "Remote host",
            &form.remote_host,
            Field::RemoteHost,
            "db.internal"
        ),
        field("Remote port", &form.remote_port, Field::RemotePort, "5432"),
    ]
    .spacing(10);

    if let Some(err) = &form.error {
        col = col.push(
            text(err.clone())
                .size(12)
                .color(Color::from_rgb(0.9, 0.3, 0.3)),
        );
    }

    let mut buttons = row![
        button(text("Save").size(13))
            .style(button::primary)
            .padding([4, 14])
            .on_press(Message::SaveEdit),
        button(text("Cancel").size(13))
            .padding([4, 14])
            .on_press(Message::CancelEdit),
    ]
    .spacing(10);
    if let Some(idx) = form.target {
        buttons = buttons.push(space().width(Length::Fill));
        buttons = buttons.push(
            button(text("Delete").size(13))
                .style(button::danger)
                .padding([4, 14])
                .on_press(Message::DeleteTunnel(idx)),
        );
    }
    col = col.push(buttons);

    container(col).padding(16).into()
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
    Subscription::batch([
        iced::time::every(Duration::from_millis(200)).map(|_| Message::Tick),
        iced::window::close_events().map(Message::WindowClosed),
    ])
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
        .theme(|_app: &App, _id| Theme::Light)
        .title(|_app: &App, _id| String::from("sshoal"))
        .run()
}
