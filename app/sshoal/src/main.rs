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
const ICON_MINUS: &str = "\u{e11c}";
const ICON_CHEVRON_LEFT: &str = "\u{e06e}";
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

/// Which tunnels the list shows — the state half of the filter bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateFilter {
    All,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone)]
enum Message {
    /// Periodic: refresh status dots and poll the tray menu channel.
    Tick,
    ToggleTunnel(usize),
    ClickFolder(String),
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
    // Selection / keyboard nav
    SelectDelta(i32),
    DeleteSelected,
    ConfirmDeleteInput(String),
    ConfirmDeleteDo,
    ConfirmDeleteCancel,
    Keyboard(iced::keyboard::Event),
    // Filter bar (tunnels screen)
    FilterInput(String),
    SetFilter(StateFilter),
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
    /// Currently selected row, by identity (tunnel/folder `path` on the tree, or
    /// ssh-config `name` on the SSH screen) — for keyboard nav + delete (−).
    selected: Option<String>,
    /// Pending folder deletion awaiting type-to-confirm.
    confirm_delete: Option<ConfirmDelete>,
    /// Free-text filter (matches tunnel name or folder path).
    filter: String,
    /// Connection-state filter.
    filter_state: StateFilter,
}

/// Type-the-name confirmation for deleting a whole folder of tunnels.
struct ConfirmDelete {
    path: String,
    name: String,
    count: usize,
    typed: String,
}

impl App {
    /// The selectable items on the current screen, in display order.
    fn selectable(&self) -> Vec<String> {
        if self.managing_ssh {
            self.ssh_configs.iter().map(|c| c.name.clone()).collect()
        } else {
            self.display_rows().into_iter().map(|d| d.path).collect()
        }
    }

    /// The tree rows currently visible (after the filter), in display order.
    /// Shared by the view and keyboard nav so they never disagree.
    fn display_rows(&self) -> Vec<DisplayRow> {
        let allowed = self.filtered_indices();
        let mut out = Vec::new();
        flatten(
            &build_tree(&self.rows),
            "",
            0,
            &self.rows,
            &self.expanded,
            allowed.as_ref(),
            &mut out,
        );
        out
    }

    /// Row indices passing the filter, or `None` when no filter is active (show
    /// everything, honouring the manual expand/collapse state).
    fn filtered_indices(&self) -> Option<HashSet<usize>> {
        let q = self.filter.trim().to_lowercase();
        if q.is_empty() && self.filter_state == StateFilter::All {
            return None;
        }
        let set = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                let name_ok = q.is_empty()
                    || r.tunnel.path.to_lowercase().contains(&q)
                    || r.tunnel.name().to_lowercase().contains(&q);
                let state_ok = match self.filter_state {
                    StateFilter::All => true,
                    StateFilter::Connected => r.enabled(),
                    StateFilter::Disconnected => !r.enabled(),
                };
                name_ok && state_ok
            })
            .map(|(i, _)| i)
            .collect();
        Some(set)
    }
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
        selected: None,
        confirm_delete: None,
        filter: String::new(),
        filter_state: StateFilter::All,
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
        Message::ClickFolder(path) => {
            app.selected = Some(path.clone());
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
            app.selected = app.rows.get(i).map(|r| r.tunnel.path.clone());
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
            app.selected = None;
            Task::none()
        }
        Message::CloseSshConfigs => {
            app.managing_ssh = false;
            app.editing_ssh = None;
            app.selected = None;
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
            app.selected = app.ssh_configs.get(i).map(|c| c.name.clone());
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
        Message::SelectDelta(delta) => {
            let items = app.selectable();
            if !items.is_empty() {
                let cur = app
                    .selected
                    .as_ref()
                    .and_then(|s| items.iter().position(|it| it == s))
                    .unwrap_or(0) as i32;
                let next = (cur + delta).rem_euclid(items.len() as i32) as usize;
                app.selected = Some(items[next].clone());
            }
            Task::none()
        }
        Message::DeleteSelected => {
            let Some(sel) = app.selected.take() else {
                return Task::none();
            };
            if app.managing_ssh {
                if let Some(i) = app.ssh_configs.iter().position(|c| c.name == sel) {
                    return update(app, Message::DeleteSsh(i));
                }
            } else if let Some(i) = app.rows.iter().position(|r| r.tunnel.path == sel) {
                // A leaf tunnel.
                return update(app, Message::DeleteTunnel(i));
            } else {
                // A folder: confirm by typing its name before deleting the lot.
                let count = descendant_indices(&app.rows, &sel).len();
                if count > 0 {
                    let name = sel.rsplit('/').next().unwrap_or(&sel).to_string();
                    app.confirm_delete = Some(ConfirmDelete {
                        path: sel,
                        name,
                        count,
                        typed: String::new(),
                    });
                }
            }
            Task::none()
        }
        Message::ConfirmDeleteInput(value) => {
            if let Some(c) = &mut app.confirm_delete {
                c.typed = value;
            }
            Task::none()
        }
        Message::ConfirmDeleteCancel => {
            app.confirm_delete = None;
            Task::none()
        }
        Message::ConfirmDeleteDo => {
            if let Some(c) = app.confirm_delete.take()
                && c.typed.trim() == c.name
            {
                for i in descendant_indices(&app.rows, &c.path).into_iter().rev() {
                    let mut row = app.rows.remove(i);
                    if let Some(sup) = row.supervisor.take() {
                        sup.cancel();
                    }
                }
                info!(folder = %c.path, "delete folder");
                app.expanded = all_folder_paths(&app.rows);
                persist(&app.rows, &app.ssh_configs);
            }
            Task::none()
        }
        Message::Keyboard(event) => {
            use iced::keyboard::{Event, Key, key::Named};
            // Only navigate the list when not typing in a form. (Backspace/Delete
            // are NOT bound to deletion here — the filter box would capture them;
            // use the − button instead.)
            if app.editing.is_none()
                && app.editing_ssh.is_none()
                && app.confirm_delete.is_none()
                && let Event::KeyPressed { key, .. } = event
            {
                return match key {
                    Key::Named(Named::ArrowUp) => update(app, Message::SelectDelta(-1)),
                    Key::Named(Named::ArrowDown) => update(app, Message::SelectDelta(1)),
                    Key::Named(Named::Enter) => activate_selected(app),
                    _ => Task::none(),
                };
            }
            Task::none()
        }
        Message::FilterInput(value) => {
            app.filter = value;
            Task::none()
        }
        Message::SetFilter(state) => {
            app.filter_state = state;
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

/// Enter on the current selection: edit an ssh config / leaf tunnel, or
/// expand/collapse a folder.
fn activate_selected(app: &mut App) -> Task<Message> {
    let Some(sel) = app.selected.clone() else {
        return Task::none();
    };
    if app.managing_ssh {
        if let Some(i) = app.ssh_configs.iter().position(|c| c.name == sel) {
            return update(app, Message::StartEditSsh(i));
        }
        return Task::none();
    }
    if let Some(i) = app.rows.iter().position(|r| r.tunnel.path == sel) {
        return update(app, Message::StartEdit(i));
    }
    update(app, Message::ClickFolder(sel))
}

/// Validate the edit form and apply it (replace or append a tunnel), then save.
fn save_edit(app: &mut App) {
    let Some(form) = &app.editing else { return };

    let parse_port = |s: &str| s.trim().parse::<u16>().ok();
    // Trim each path segment so " gc / dev / app-api " becomes "gc/dev/app-api"
    // and the leaf name has no stray leading/trailing spaces.
    let path = form
        .path
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("/");
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
    allowed: Option<&HashSet<usize>>,
    out: &mut Vec<DisplayRow>,
) {
    for (name, sub) in &folder.subfolders {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        // When filtering, drop folders with no matching descendant and aggregate
        // only over the matching leaves.
        let leaves: Vec<usize> = match allowed {
            Some(allow) => collect_leaves(sub)
                .into_iter()
                .filter(|i| allow.contains(i))
                .collect(),
            None => collect_leaves(sub),
        };
        if allowed.is_some() && leaves.is_empty() {
            continue;
        }
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
        // A filter forces folders open so matches are always visible.
        if allowed.is_some() || expanded.contains(&path) {
            flatten(sub, &path, depth + 1, rows, expanded, allowed, out);
        }
    }
    for &idx in &folder.leaves {
        if allowed.is_some_and(|allow| !allow.contains(&idx)) {
            continue;
        }
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
    if let Some(c) = &app.confirm_delete {
        return confirm_view(c);
    }
    if let Some(form) = &app.editing_ssh {
        return ssh_edit_view(form);
    }
    if app.managing_ssh {
        return ssh_list_view(app);
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
        row![
            tip(
                icon_button(ICON_PLUS, 19.0, Message::StartAdd),
                "Add tunnel"
            ),
            tip(
                icon_button(ICON_MINUS, 19.0, Message::DeleteSelected),
                "Delete selected"
            ),
        ]
        .spacing(8),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let display = app.display_rows();
    let mut list = column![].spacing(2);
    if app.rows.is_empty() {
        list = list.push(text("No tunnels yet. Click + Add, or run `sshoal import-ssh`.").size(13));
    } else if display.is_empty() {
        list = list.push(
            text("No tunnels match the filter.")
                .size(13)
                .color(Color::from_rgb(0.5, 0.5, 0.56)),
        );
    }
    for d in &display {
        list = list.push(tree_row(app, d));
    }

    let body = scrollable(list).height(Length::Fill);
    let mut screen = column![header].spacing(10);
    if !app.rows.is_empty() {
        screen = screen.push(filter_bar(app));
    }
    screen = screen.push(body);
    container(screen).padding(14).into()
}

/// The filter bar: a free-text search (name or folder) plus state chips.
fn filter_bar(app: &App) -> Element<'_, Message> {
    let search = text_input("filter…", &app.filter)
        .size(12)
        .padding([5, 9])
        .style(rounded_input)
        .on_input(Message::FilterInput)
        .width(Length::Fill);

    let chip = |label: &'static str, state: StateFilter| {
        let active = app.filter_state == state;
        button(text(label).size(11))
            .style(move |_t: &iced::Theme, status| chip_style(active, status))
            .padding([4, 10])
            .on_press(Message::SetFilter(state))
    };

    row![
        search,
        chip("All", StateFilter::All),
        chip("Connected", StateFilter::Connected),
        chip("Disconnected", StateFilter::Disconnected),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center)
    .into()
}

fn tree_row<'a>(app: &App, d: &DisplayRow) -> Element<'a, Message> {
    let indent = space().width(Length::Fixed(d.depth as f32 * 16.0));

    // Folder: a band you click to expand/collapse. No toggle/status. Selection
    // is shown by an accent border, not by repainting the whole row.
    let Some(idx) = d.row_idx else {
        let expanded = app.expanded.contains(&d.path);
        // Closed folders get a warm fill so they read as "has hidden contents";
        // open ones fade to a muted grey.
        let (icon, icon_color) = if expanded {
            (ICON_FOLDER_OPEN, Color::from_rgb(0.55, 0.60, 0.68))
        } else {
            (ICON_FOLDER, Color::from_rgb(0.93, 0.69, 0.22))
        };
        let content = row![
            indent,
            text(icon).font(LUCIDE).size(16).color(icon_color),
            name_element(&d.name, 14.0, 26),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);
        let selected = app.selected.as_deref() == Some(d.path.as_str());
        return button(content)
            .style(if selected {
                folder_selected
            } else {
                folder_button
            })
            .width(Length::Fill)
            .padding([5, 6])
            .on_press(Message::ClickFolder(d.path.clone()))
            .into();
    };

    // Leaf: clicking the row opens edit; the toggler (sibling) connects/disconnects.
    let selected = app.selected.as_deref() == Some(d.path.as_str());
    let label = row![
        indent,
        status_dot(d.status),
        name_element(&d.name, 13.0, 26),
    ]
    .spacing(10)
    .align_y(iced::Alignment::Center);
    let click = button(label)
        .style(if selected { row_selected } else { row_plain })
        .width(Length::Fill)
        .padding([3, 6])
        .on_press(Message::StartEdit(idx));
    let switch = tip(
        toggler(d.enabled)
            .size(18)
            .on_toggle(move |_| Message::ToggleTunnel(idx)),
        if d.enabled { "Disconnect" } else { "Connect" },
    );
    let line = row![click, switch, space().width(Length::Fixed(8.0))]
        .spacing(8)
        .align_y(iced::Alignment::Center);

    let mut col = column![line].spacing(1);
    if let Some((msg, _)) = &app.rows[idx].notice {
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

fn row_style(bg: Option<Color>, radius: f32) -> iced::widget::button::Style {
    iced::widget::button::Style {
        background: bg.map(iced::Background::Color),
        text_color: Color::from_rgb(0.13, 0.13, 0.18),
        border: iced::Border {
            radius: radius.into(),
            ..iced::Border::default()
        },
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

/// Folder row: a slightly darker band so folders stand out from leaves.
fn folder_button(_theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let shade = match status {
        button::Status::Hovered => 0.82,
        button::Status::Pressed => 0.78,
        _ => 0.86,
    };
    row_style(Some(Color::from_rgb(shade, shade, shade + 0.03)), 0.0)
}

/// Folder row, selected: keep the band, add a blue accent border (rather than
/// repainting the whole row a solid colour).
fn folder_selected(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let mut style = folder_button(theme, status);
    style.border = iced::Border {
        color: Color::from_rgb(0.36, 0.56, 0.96),
        width: 1.5,
        radius: 5.0.into(),
    };
    style
}

/// Leaf row, not selected: transparent, faint highlight on hover.
fn row_plain(_theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Color::from_rgb(0.95, 0.95, 0.97)),
        _ => None,
    };
    row_style(bg, 6.0)
}

/// Leaf row, selected: a light blue highlight.
fn row_selected(_theme: &iced::Theme, _status: button::Status) -> iced::widget::button::Style {
    row_style(Some(Color::from_rgb(0.80, 0.87, 1.0)), 6.0)
}

/// Wrap a control with a hover tooltip.
fn tip<'a>(content: impl Into<Element<'a, Message>>, label: &'a str) -> Element<'a, Message> {
    tip_text(content, label.to_string())
}

/// Like [`tip`] but takes an owned label (so it can outlive a borrowed source).
fn tip_text<'a>(content: impl Into<Element<'a, Message>>, label: String) -> Element<'a, Message> {
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

/// Truncate to `max` characters with an ellipsis (counted in `char`s, so it
/// never splits a multi-byte glyph).
fn truncate(name: &str, max: usize) -> String {
    if name.chars().count() > max {
        let kept: String = name.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        name.to_string()
    }
}

/// A row label that truncates long names to `max` chars and, when truncated,
/// reveals the full name in a hover tooltip.
fn name_element<'a>(name: &str, size: f32, max: usize) -> Element<'a, Message> {
    let shown = truncate(name, max);
    if shown == name {
        text(shown).size(size).width(Length::Fill).into()
    } else {
        tip_text(
            container(text(shown).size(size)).width(Length::Fill),
            name.to_string(),
        )
    }
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

/// Rounded "pill" buttons — one per visual role, all sharing the Add button's
/// rounded shape so the UI is consistent.
fn pill_button(theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    pill(button::primary(theme, status))
}

/// Light, bordered secondary button (reads clearly as a button, not flat).
fn pill_secondary(_theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let shade = match status {
        button::Status::Hovered => 0.90,
        button::Status::Pressed => 0.84,
        _ => 0.95,
    };
    iced::widget::button::Style {
        background: Some(iced::Background::Color(Color::from_rgb(
            shade,
            shade,
            shade + 0.01,
        ))),
        text_color: Color::from_rgb(0.18, 0.18, 0.22),
        border: iced::Border {
            color: Color::from_rgb(0.76, 0.76, 0.80),
            width: 1.0,
            radius: 14.0.into(),
        },
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

/// Filter-bar chip: filled blue when active, light bordered pill otherwise.
fn chip_style(active: bool, status: button::Status) -> iced::widget::button::Style {
    let (bg, fg, border_w) = if active {
        (Color::from_rgb(0.36, 0.56, 0.96), Color::WHITE, 0.0)
    } else {
        let shade = match status {
            button::Status::Hovered => 0.90,
            button::Status::Pressed => 0.84,
            _ => 0.95,
        };
        (
            Color::from_rgb(shade, shade, shade + 0.01),
            Color::from_rgb(0.30, 0.30, 0.36),
            1.0,
        )
    };
    iced::widget::button::Style {
        background: Some(iced::Background::Color(bg)),
        text_color: fg,
        border: iced::Border {
            color: Color::from_rgb(0.76, 0.76, 0.80),
            width: border_w,
            radius: 11.0.into(),
        },
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

/// Rounded text-input style (Tahoe-ish).
fn rounded_input(
    theme: &iced::Theme,
    status: iced::widget::text_input::Status,
) -> iced::widget::text_input::Style {
    let mut style = iced::widget::text_input::default(theme, status);
    style.border = iced::Border {
        radius: 9.0.into(),
        ..style.border
    };
    style
}

/// Rounded pick_list (dropdown) field — matches the rounded text inputs.
fn rounded_pick(
    theme: &iced::Theme,
    status: iced::widget::pick_list::Status,
) -> iced::widget::pick_list::Style {
    let mut style = iced::widget::pick_list::default(theme, status);
    style.border = iced::Border {
        radius: 9.0.into(),
        ..style.border
    };
    style
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
                .padding([6, 9])
                .style(rounded_input)
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
        .padding([6, 9])
        .text_size(13)
        .style(rounded_pick)
        .width(Length::Fill),
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

fn ssh_list_view(app: &App) -> Element<'_, Message> {
    let header = row![
        tip(
            icon_button(ICON_CHEVRON_LEFT, 20.0, Message::CloseSshConfigs),
            "Back"
        ),
        text("SSH configs").size(20).width(Length::Fill),
        row![
            tip(
                icon_button(ICON_PLUS, 19.0, Message::StartAddSsh),
                "Add SSH config"
            ),
            tip(
                icon_button(ICON_MINUS, 19.0, Message::DeleteSelected),
                "Delete selected"
            ),
        ]
        .spacing(8),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let mut list = column![].spacing(2);
    if app.ssh_configs.is_empty() {
        list = list
            .push(text("No SSH configs yet. Click + to add, or run `sshoal import-ssh`.").size(13));
    }
    for (i, c) in app.ssh_configs.iter().enumerate() {
        let user = c.user.as_deref().unwrap_or("-");
        let sub = format!("{user}@{}:{}", c.host, c.port);
        let content = column![
            text(c.name.clone()).size(14),
            text(sub).size(11).color(Color::from_rgb(0.5, 0.5, 0.56)),
        ]
        .spacing(2)
        .width(Length::Fill);
        let selected = app.selected.as_deref() == Some(c.name.as_str());
        list = list.push(
            button(content)
                .style(if selected { row_selected } else { row_plain })
                .width(Length::Fill)
                .padding([6, 8])
                .on_press(Message::StartEditSsh(i)),
        );
    }

    container(column![header, scrollable(list).height(Length::Fill)].spacing(12))
        .padding(14)
        .into()
}

/// A borderless icon button (lucide glyph) that gets a rounded-square highlight
/// on hover. Used for the +/− and back header actions.
fn icon_button<'a>(icon: &'a str, size: f32, msg: Message) -> Element<'a, Message> {
    button(text(icon).font(LUCIDE).size(size))
        .style(icon_btn_style)
        .padding([4, 7])
        .on_press(msg)
        .into()
}

fn icon_btn_style(_theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Color::from_rgb(0.90, 0.90, 0.92)),
        button::Status::Pressed => Some(Color::from_rgb(0.83, 0.83, 0.86)),
        _ => None,
    };
    row_style(bg, 7.0)
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
                .padding([6, 9])
                .style(rounded_input)
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

fn confirm_view(c: &ConfirmDelete) -> Element<'_, Message> {
    let matches = c.typed.trim() == c.name;
    let mut col = column![
        text("Delete folder").size(18),
        text(format!(
            "This permanently deletes “{}” and its {} tunnel(s).",
            c.path, c.count
        ))
        .size(13),
        text(format!("Type “{}” to confirm:", c.name)).size(13),
        text_input(&c.name, &c.typed)
            .size(13)
            .padding([6, 9])
            .style(rounded_input)
            .on_input(Message::ConfirmDeleteInput),
    ]
    .spacing(10);

    let mut delete = button(text("Delete").size(13))
        .style(pill_danger)
        .padding([5, 16]);
    if matches {
        delete = delete.on_press(Message::ConfirmDeleteDo);
    }
    col = col.push(
        row![
            delete,
            button(text("Cancel").size(13))
                .style(pill_secondary)
                .padding([5, 16])
                .on_press(Message::ConfirmDeleteCancel),
        ]
        .spacing(10),
    );
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
        iced::keyboard::listen().map(Message::Keyboard),
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
        size: Size::new(380.0, 620.0),
        min_size: Some(Size::new(320.0, 380.0)),
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
