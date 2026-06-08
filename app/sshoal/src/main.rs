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

use iced::widget::{
    button, column, container, pick_list, row, scrollable, space, text, text_input, toggler,
    tooltip,
};
use iced::{Color, Element, Font, Length, Size, Subscription, Task, Theme, window};

/// Lucide icon font (bundled) — iced can't render colour emoji, so we use a
/// monochrome icon font for crisp Add/Edit/folder glyphs.
const LUCIDE: Font = Font::with_name("lucide");
const ICON_PLUS: &str = "\u{e13d}";
const ICON_PENCIL: &str = "\u{e1f9}";
const ICON_FOLDER: &str = "\u{e0d7}";
const ICON_FOLDER_OPEN: &str = "\u{e247}";
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use sshoal_core::{
    AppConfig, Backoff, OpenSshTransport, SshConfig, Transport, Tunnel, TunnelState,
    TunnelSupervisor,
};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, Copy)]
enum Field {
    Path,
    Ssh,
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
    // SSH config management
    OpenSshConfigs,
    CloseSshConfigs,
    StartAddSsh,
    StartEditSsh(usize),
    EditSshField(SshField, String),
    SaveSsh,
    CancelSsh,
    DeleteSsh(usize),
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
    /// Transient reason shown under the row (failure / conflict), with the time
    /// it was set so it can auto-dismiss.
    notice: Option<(String, std::time::Instant)>,
    /// Last error surfaced from the supervisor, to detect *new* failures.
    err_seen: Option<String>,
}

impl TunnelRow {
    fn enabled(&self) -> bool {
        self.supervisor.is_some()
    }

    fn local_port(&self) -> u16 {
        self.tunnel.local_port
    }
}

/// In-progress add/edit form. Ports are kept as strings while typing.
#[derive(Default)]
struct EditForm {
    target: Option<usize>, // Some(idx) = editing existing, None = adding new
    path: String,
    ssh: String,
    local_port: String,
    remote_host: String,
    remote_port: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum SshField {
    Name,
    Host,
    Port,
    User,
    Identity,
}

/// In-progress add/edit form for an SSH config.
#[derive(Default)]
struct SshForm {
    target: Option<usize>,
    name: String,
    host: String,
    port: String,
    user: String,
    identity: String,
    error: Option<String>,
}

struct App {
    /// Created lazily on the first tick — on macOS the tray must be created
    /// after the app's event loop is running, not during `boot`.
    tray: Option<TrayIcon>,
    menu: Option<MenuIds>,
    /// Global hotkey (⌃⌘S) to summon the window — reliable even when the tray
    /// icon is hidden behind the notch on a crowded menu bar.
    _hotkey: Option<GlobalHotKeyManager>,
    window: Option<window::Id>,
    runtime: Arc<tokio::runtime::Runtime>,
    transport: Arc<dyn Transport>,
    ssh_configs: Vec<SshConfig>,
    rows: Vec<TunnelRow>,
    expanded: HashSet<String>,
    editing: Option<EditForm>,
    /// Showing the SSH-configs list screen.
    managing_ssh: bool,
    /// In-progress add/edit of an SSH config.
    editing_ssh: Option<SshForm>,
}

impl App {
    fn ssh_names(&self) -> Vec<String> {
        self.ssh_configs.iter().map(|c| c.name.clone()).collect()
    }

    /// Resolve the ssh config a tunnel uses (falling back to a bare alias).
    fn resolve_ssh(&self, tunnel: &Tunnel) -> SshConfig {
        self.ssh_configs
            .iter()
            .find(|c| c.name == tunnel.ssh)
            .cloned()
            .unwrap_or_else(|| SshConfig::alias(&tunnel.ssh))
    }
}

fn boot(runtime: Arc<tokio::runtime::Runtime>) -> (App, Task<Message>) {
    // Tunnels shouldn't outlive the app — clean up any orphaned ssh from a
    // previous run that was force-killed (so their local ports are free again).
    kill_stale_tunnels();

    let path = config_path();
    ensure_example(&path);
    let config = AppConfig::load(&path).unwrap_or_else(|err| {
        tracing::error!(error = %err, path = %path.display(), "failed to load config");
        AppConfig::default()
    });
    info!(path = %path.display(), tunnels = config.tunnels.len(), "config loaded");

    let ssh_configs = config.ssh_configs;
    let rows: Vec<TunnelRow> = config
        .tunnels
        .into_iter()
        .map(|tunnel| TunnelRow {
            tunnel,
            supervisor: None,
            status: TunnelState::Idle,
            notice: None,
            err_seen: None,
        })
        .collect();
    let expanded = all_folder_paths(&rows);

    let mut app = App {
        tray: None,
        menu: None,
        _hotkey: None,
        window: None,
        runtime,
        transport: Arc::new(OpenSshTransport),
        ssh_configs,
        rows,
        expanded,
        editing: None,
        managing_ssh: false,
        editing_ssh: None,
    };

    // Show the window on launch so opening the app always surfaces it (the tray
    // icon can be hard to spot); `gain_focus` brings it to the front since we
    // run as a menu-bar accessory with no Dock icon.
    let (id, open_task) = window::open(open_window_settings());
    app.window = Some(id);
    let task = Task::batch([open_task.map(Message::WindowOpened), window::gain_focus(id)]);
    (app, task)
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            for row in &mut app.rows {
                let err = if let Some(sup) = &row.supervisor {
                    row.status = sup.state();
                    sup.last_error()
                } else {
                    None
                };
                // Surface a *new* failure as a transient reason under the row.
                if err != row.err_seen {
                    row.err_seen = err.clone();
                    if let Some(msg) = err {
                        row.notice = Some((msg, std::time::Instant::now()));
                    }
                }
                // Auto-dismiss after 3s.
                if let Some((_, shown)) = &row.notice
                    && shown.elapsed() > Duration::from_secs(3)
                {
                    row.notice = None;
                }
            }

            // Create the tray icon once the event loop is running — on macOS it
            // must not be created during `boot`, or it never appears.
            if app.tray.is_none() {
                let (tray, menu) = build_tray();
                app.tray = Some(tray);
                app.menu = Some(menu);
                app._hotkey = register_hotkey();
            }

            let ids = app
                .menu
                .as_ref()
                .map(|m| (m.open.clone(), m.quit.clone(), m.connect_all.clone()));
            let rx = MenuEvent::receiver();
            while let Ok(event) = rx.try_recv() {
                let Some((open, quit, connect_all)) = ids.as_ref() else {
                    break;
                };
                if event.id == *open {
                    if app.window.is_none() {
                        let (id, task) = window::open(open_window_settings());
                        app.window = Some(id);
                        return Task::batch([
                            task.map(Message::WindowOpened),
                            window::gain_focus(id),
                        ]);
                    } else if let Some(id) = app.window {
                        return window::gain_focus(id);
                    }
                } else if event.id == *quit {
                    info!("quitting");
                    return iced::exit();
                } else if event.id == *connect_all {
                    for i in 0..app.rows.len() {
                        set_enabled(app, i, true);
                    }
                }
            }

            // Global hotkey (⌃⌘S) → summon the window.
            let hk_rx = GlobalHotKeyEvent::receiver();
            while let Ok(event) = hk_rx.try_recv() {
                if event.state == HotKeyState::Pressed {
                    if app.window.is_none() {
                        let (id, task) = window::open(open_window_settings());
                        app.window = Some(id);
                        return Task::batch([
                            task.map(Message::WindowOpened),
                            window::gain_focus(id),
                        ]);
                    } else if let Some(id) = app.window {
                        return window::gain_focus(id);
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
            // The folder switch shows ON if *any* child is on, so clicking it
            // when any are on should turn the whole folder OFF.
            let any_on = indices.iter().any(|&i| app.rows[i].enabled());
            for i in indices {
                set_enabled(app, i, !any_on);
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
                persist(&app.rows, &app.ssh_configs);
            }
            Task::none()
        }
        Message::OpenSshConfigs => {
            app.managing_ssh = true;
            app.editing = None;
            Task::none()
        }
        Message::CloseSshConfigs => {
            app.managing_ssh = false;
            app.editing_ssh = None;
            Task::none()
        }
        Message::StartAddSsh => {
            app.editing_ssh = Some(SshForm {
                port: "22".to_string(),
                ..SshForm::default()
            });
            Task::none()
        }
        Message::StartEditSsh(i) => {
            if let Some(c) = app.ssh_configs.get(i) {
                app.editing_ssh = Some(SshForm {
                    target: Some(i),
                    name: c.name.clone(),
                    host: c.host.clone(),
                    port: c.port.to_string(),
                    user: c.user.clone().unwrap_or_default(),
                    identity: c.identity_file.clone().unwrap_or_default(),
                    error: None,
                });
            }
            Task::none()
        }
        Message::EditSshField(field, value) => {
            if let Some(form) = &mut app.editing_ssh {
                match field {
                    SshField::Name => form.name = value,
                    SshField::Host => form.host = value,
                    SshField::Port => form.port = value,
                    SshField::User => form.user = value,
                    SshField::Identity => form.identity = value,
                }
            }
            Task::none()
        }
        Message::SaveSsh => {
            save_ssh(app);
            Task::none()
        }
        Message::CancelSsh => {
            app.editing_ssh = None;
            Task::none()
        }
        Message::DeleteSsh(i) => {
            app.editing_ssh = None;
            if i < app.ssh_configs.len() {
                let removed = app.ssh_configs.remove(i);
                info!(ssh = %removed.name, "delete ssh config");
                persist(&app.rows, &app.ssh_configs);
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
            notice: None,
            err_seen: None,
        }),
    }

    app.editing = None;
    app.expanded = all_folder_paths(&app.rows);
    persist(&app.rows, &app.ssh_configs);
}

/// Write the current ssh configs + tunnels back to the config file.
fn persist(rows: &[TunnelRow], ssh_configs: &[SshConfig]) {
    let config = AppConfig {
        ssh_configs: ssh_configs.to_vec(),
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

    // Refuse to bring up a tunnel whose local port is already taken by another
    // active tunnel — that would just fail and spin forever.
    if on && app.rows.get(i).is_some_and(|r| !r.enabled()) {
        let port = app.rows[i].local_port();
        if let Some(j) = (0..app.rows.len())
            .find(|&j| j != i && app.rows[j].enabled() && app.rows[j].local_port() == port)
        {
            let msg = format!(
                "Local port {port} is already in use by “{}”.",
                app.rows[j].tunnel.path
            );
            app.rows[i].notice = Some((msg, std::time::Instant::now()));
            return;
        }
    }

    let ssh = match app.rows.get(i) {
        Some(r) => app.resolve_ssh(&r.tunnel),
        None => return,
    };
    let Some(row) = app.rows.get_mut(i) else {
        return;
    };

    if on && row.supervisor.is_none() {
        let _guard = runtime.enter();
        let sup = TunnelSupervisor::spawn(transport, row.tunnel.clone(), ssh, Backoff::default());
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
    if let Some(form) = &app.editing_ssh {
        return ssh_edit_view(form);
    }
    if app.managing_ssh {
        return ssh_list_view(&app.ssh_configs);
    }
    if let Some(form) = &app.editing {
        return edit_view(form, &app.ssh_names());
    }

    let header = row![
        text("sshoal").size(22).width(Length::Fill),
        button(text("SSH configs").size(12))
            .style(pill_secondary)
            .padding([4, 10])
            .on_press(Message::OpenSshConfigs),
        tip(
            button(text(ICON_PLUS).font(LUCIDE).size(18))
                .style(button::text)
                .padding([2, 8])
                .on_press(Message::StartAdd),
            "Add tunnel",
        ),
    ]
    .spacing(8)
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
    container(column![header, body].spacing(10))
        .padding(14)
        .into()
}

fn tree_row<'a>(app: &App, d: &DisplayRow) -> Element<'a, Message> {
    let indent = space().width(Length::Fixed(d.depth as f32 * 16.0));
    let is_folder = d.row_idx.is_none();

    let lead: Element<Message> = if is_folder {
        let expanded = app.expanded.contains(&d.path);
        let icon = if expanded {
            ICON_FOLDER_OPEN
        } else {
            ICON_FOLDER
        };
        tip(
            button(text(icon).font(LUCIDE).size(15))
                .style(button::text)
                .padding(2)
                .on_press(Message::ExpandCollapse(d.path.clone())),
            if expanded { "Collapse" } else { "Expand" },
        )
    } else {
        status_dot(d.status)
    };

    let name: Element<Message> = if is_folder {
        text(d.name.clone()).size(14).width(Length::Fill).into()
    } else {
        text(d.name.clone()).size(13).width(Length::Fill).into()
    };

    // On/off switch: cleaner than a text button, and doesn't read as up/down.
    let switch: Element<Message> = if is_folder {
        let path = d.path.clone();
        tip(
            toggler(d.enabled)
                .size(18)
                .on_toggle(move |_| Message::ToggleFolder(path.clone())),
            if d.enabled {
                "Disconnect all"
            } else {
                "Connect all"
            },
        )
    } else {
        let idx = d.row_idx.unwrap();
        tip(
            toggler(d.enabled)
                .size(18)
                .on_toggle(move |_| Message::ToggleTunnel(idx)),
            if d.enabled { "Disconnect" } else { "Connect" },
        )
    };

    let mut line = row![indent, lead, name].spacing(10);
    if is_folder {
        line = line.push(status_dot(d.status));
    } else if let Some(idx) = d.row_idx {
        line = line.push(tip(
            button(text(ICON_PENCIL).font(LUCIDE).size(15))
                .style(button::text)
                .padding([2, 6])
                .on_press(Message::StartEdit(idx)),
            "Edit tunnel",
        ));
    }
    line = line.push(switch);
    // Keep controls clear of the scrollbar on the right.
    line = line.push(space().width(Length::Fixed(8.0)));

    let line = line.align_y(iced::Alignment::Center);
    if is_folder {
        // Highlight folder rows with a subtle background band.
        return container(line)
            .width(Length::Fill)
            .padding([5, 6])
            .style(folder_band)
            .into();
    }

    // Leaf: optionally show a transient reason line underneath.
    let mut col = column![container(line).padding([3, 6])].spacing(1);
    if let Some(idx) = d.row_idx
        && let Some((msg, _)) = &app.rows[idx].notice
    {
        col = col.push(
            row![
                space().width(Length::Fixed(d.depth as f32 * 16.0 + 26.0)),
                text(msg.clone())
                    .size(11)
                    .color(Color::from_rgb(0.80, 0.40, 0.16)),
            ]
            .spacing(4),
        );
    }
    col.into()
}

/// Wrap a control with a hover tooltip.
fn tip<'a>(content: impl Into<Element<'a, Message>>, label: &'a str) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(label).size(12))
            .padding([4, 8])
            .style(tooltip_bubble),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .into()
}

fn tooltip_bubble(_theme: &iced::Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.15, 0.15, 0.18))),
        text_color: Some(Color::from_rgb(0.96, 0.96, 0.98)),
        border: iced::Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
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

/// Rounded "pill" buttons — one per visual role, all sharing the Add button's
/// rounded shape so the UI is consistent.
fn pill_button(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    pill(button::primary(theme, status))
}

fn pill_secondary(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    pill(button::secondary(theme, status))
}

fn pill_danger(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    pill(button::danger(theme, status))
}

fn pill(base: iced::widget::button::Style) -> iced::widget::button::Style {
    iced::widget::button::Style {
        border: iced::Border {
            radius: 14.0.into(),
            ..base.border
        },
        ..base
    }
}

fn edit_view<'a>(form: &'a EditForm, ssh_names: &[String]) -> Element<'a, Message> {
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

    // SSH config: a dropdown of known configs (plus the current value if it
    // names an alias that isn't a saved config).
    let mut options = ssh_names.to_vec();
    if !form.ssh.is_empty() && !options.contains(&form.ssh) {
        options.push(form.ssh.clone());
    }
    let selected = (!form.ssh.is_empty()).then(|| form.ssh.clone());
    let ssh_field: Element<Message> = row![
        text("SSH config").size(13).width(Length::Fixed(110.0)),
        pick_list(options, selected, |name| {
            Message::EditField(Field::Ssh, name)
        })
        .placeholder("choose an SSH config")
        .text_size(13),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .into();

    let mut col = column![
        text(title).size(18),
        field("Path", &form.path, Field::Path, "gc/dev/db/app-api"),
        ssh_field,
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
            .style(pill_button)
            .padding([5, 16])
            .on_press(Message::SaveEdit),
        button(text("Cancel").size(13))
            .style(pill_secondary)
            .padding([5, 16])
            .on_press(Message::CancelEdit),
    ]
    .spacing(10);
    if let Some(idx) = form.target {
        buttons = buttons.push(space().width(Length::Fill));
        buttons = buttons.push(
            button(text("Delete").size(13))
                .style(pill_danger)
                .padding([5, 16])
                .on_press(Message::DeleteTunnel(idx)),
        );
    }
    col = col.push(buttons);

    container(col).padding(16).into()
}

fn save_ssh(app: &mut App) {
    let Some(form) = &app.editing_ssh else {
        return;
    };
    let name = form.name.trim().to_string();
    let host = form.host.trim().to_string();
    let port_str = form.port.trim().to_string();
    let user = nonempty(&form.user);
    let identity = nonempty(&form.identity);
    let target = form.target;

    let error = if name.is_empty() {
        Some("Name is required")
    } else if host.is_empty() {
        Some("Host is required")
    } else if !port_str.is_empty() && port_str.parse::<u16>().is_err() {
        Some("Port must be 1–65535")
    } else if app
        .ssh_configs
        .iter()
        .enumerate()
        .any(|(i, c)| c.name == name && Some(i) != target)
    {
        Some("A config with that name already exists")
    } else {
        None
    };

    if let Some(msg) = error {
        if let Some(form) = &mut app.editing_ssh {
            form.error = Some(msg.to_string());
        }
        return;
    }

    let config = SshConfig {
        name,
        host,
        port: port_str.parse().unwrap_or(22),
        user,
        identity_file: identity,
    };
    match target {
        Some(i) if i < app.ssh_configs.len() => app.ssh_configs[i] = config,
        _ => app.ssh_configs.push(config),
    }
    app.editing_ssh = None;
    persist(&app.rows, &app.ssh_configs);
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn ssh_list_view(configs: &[SshConfig]) -> Element<'_, Message> {
    let header = row![
        button(text("‹ Back").size(13))
            .style(pill_secondary)
            .padding([4, 12])
            .on_press(Message::CloseSshConfigs),
        text("SSH configs").size(20).width(Length::Fill),
        tip(
            button(text(ICON_PLUS).font(LUCIDE).size(18))
                .style(button::text)
                .padding([2, 8])
                .on_press(Message::StartAddSsh),
            "Add SSH config",
        ),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let mut list = column![].spacing(4);
    if configs.is_empty() {
        list = list
            .push(text("No SSH configs yet. Click + to add, or run `sshoal import-ssh`.").size(13));
    }
    for (i, c) in configs.iter().enumerate() {
        let user = c.user.as_deref().unwrap_or("-");
        let sub = format!("{user}@{}:{}", c.host, c.port);
        let line = row![
            column![
                text(c.name.clone()).size(14),
                text(sub).size(11).color(Color::from_rgb(0.55, 0.55, 0.6)),
            ]
            .spacing(2)
            .width(Length::Fill),
            tip(
                button(text(ICON_PENCIL).font(LUCIDE).size(15))
                    .style(button::text)
                    .padding([2, 6])
                    .on_press(Message::StartEditSsh(i)),
                "Edit",
            ),
            tip(
                button(text("✕").size(11))
                    .style(pill_danger)
                    .padding([2, 8])
                    .on_press(Message::DeleteSsh(i)),
                "Delete",
            ),
            space().width(Length::Fixed(8.0)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);
        list = list.push(container(line).padding([4, 6]));
    }

    container(column![header, scrollable(list).height(Length::Fill)].spacing(12))
        .padding(14)
        .into()
}

fn ssh_edit_view(form: &SshForm) -> Element<'_, Message> {
    let title = if form.target.is_some() {
        "Edit SSH config"
    } else {
        "New SSH config"
    };

    let field = |label: &str, value: &str, f: SshField, placeholder: &str| -> Element<Message> {
        row![
            text(label.to_string()).size(13).width(Length::Fixed(110.0)),
            text_input(placeholder, value)
                .size(13)
                .on_input(move |s| Message::EditSshField(f, s)),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
    };

    let mut col = column![
        text(title).size(18),
        field("Name", &form.name, SshField::Name, "gemx-dev"),
        field("Host", &form.host, SshField::Host, "1.2.3.4 or alias"),
        field("Port", &form.port, SshField::Port, "22"),
        field("User", &form.user, SshField::User, "(optional)"),
        field(
            "Key file",
            &form.identity,
            SshField::Identity,
            "~/.ssh/id_ed25519 (optional)"
        ),
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
            .style(pill_button)
            .padding([5, 16])
            .on_press(Message::SaveSsh),
        button(text("Cancel").size(13))
            .style(pill_secondary)
            .padding([5, 16])
            .on_press(Message::CancelSsh),
    ]
    .spacing(10);
    if let Some(idx) = form.target {
        buttons = buttons.push(space().width(Length::Fill));
        buttons = buttons.push(
            button(text("Delete").size(13))
                .style(pill_danger)
                .padding([5, 16])
                .on_press(Message::DeleteSsh(idx)),
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

/// Kill orphaned sshoal tunnels from a previous run. Matches our exact ssh
/// option signature so it never touches the user's own ssh sessions (e.g. a
/// plain `ssh -L` or an opentunnels.sh tunnel).
fn kill_stale_tunnels() {
    let signature = "ServerAliveInterval=15 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes -o ConnectTimeout=10";
    let _ = std::process::Command::new("pkill")
        .args(["-f", signature])
        .status();
}

/// Register the ⌃⌘S global hotkey. Best-effort: returns the manager (which must
/// be kept alive) even if registration fails.
fn register_hotkey() -> Option<GlobalHotKeyManager> {
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "global hotkey manager unavailable");
            return None;
        }
    };
    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SUPER), Code::KeyS);
    match manager.register(hotkey) {
        Ok(()) => info!("global hotkey Ctrl+Cmd+S registered (opens sshoal)"),
        Err(e) => tracing::warn!(error = %e, "failed to register global hotkey"),
    }
    Some(manager)
}

fn open_window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(420.0, 620.0),
        min_size: Some(Size::new(340.0, 380.0)),
        ..window::Settings::default()
    }
}

fn build_tray() -> (TrayIcon, MenuIds) {
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
        .build();
    match tray {
        Ok(tray) => {
            info!("tray icon created");
            (tray, menu)
        }
        Err(e) => {
            tracing::error!(error = %e, "FAILED to create tray icon");
            panic!("tray: {e}");
        }
    }
}

/// Render the bundled SVG logo to an RGBA tray icon.
fn make_icon() -> Icon {
    const SVG: &str = include_str!("../assets/icon.svg");
    let size: u32 = 64;

    let tree =
        resvg::usvg::Tree::from_str(SVG, &resvg::usvg::Options::default()).expect("parse icon svg");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("alloc pixmap");
    let scale = size as f32 / 512.0;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia stores premultiplied alpha; tray-icon wants straight RGBA.
    let mut rgba = pixmap.take();
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            px[0] = (px[0] as u32 * 255 / a) as u8;
            px[1] = (px[1] as u32 * 255 / a) as u8;
            px[2] = (px[2] as u32 * 255 / a) as u8;
        }
    }
    Icon::from_rgba(rgba, size, size).expect("valid rgba icon")
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
        .font(include_bytes!("../assets/lucide.ttf").as_slice())
        .subscription(subscription)
        .theme(|_app: &App, _id| Theme::Light)
        .title(|_app: &App, _id| String::from("sshoal"))
        .run()
}
