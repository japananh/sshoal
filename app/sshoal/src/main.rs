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
    Text, button, column, container, mouse_area, pick_list, row, scrollable, space, stack, text,
    text_input, toggler, tooltip,
};
use iced::{Color, Element, Font, Length, Size, Subscription, Task, Theme, window};

/// Lucide icon font (bundled) — iced can't render colour emoji, so we use a
/// monochrome icon font for crisp Add/Edit/folder glyphs.
const LUCIDE: Font = Font::with_name("lucide");
const ICON_PLUS: &str = "\u{e13d}";
const ICON_CHEVRON_LEFT: &str = "\u{e06e}";
const ICON_FOLDER: &str = "\u{e0d7}";
const ICON_FOLDER_OPEN: &str = "\u{e247}";
const ICON_TERMINAL: &str = "\u{e181}";
const ICON_SETTINGS: &str = "\u{e154}";
const ICON_SLIDERS: &str = "\u{e29a}";
const ICON_SEARCH: char = '\u{e151}';
/// The running version, compared against the newest GitHub release tag.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Where the "Report a bug" button sends the user.
const ISSUES_URL: &str = "https://github.com/japananh/sshoal/issues/new";
/// Blue used for the folder glyph (and a selected folder's name).
const FOLDER_BLUE: Color = Color::from_rgb(0.20, 0.50, 0.95);
/// Default dark row text.
const TEXT_DARK: Color = Color::from_rgb(0.13, 0.13, 0.18);
/// Secondary text: captions, sub-labels, section headings, status lines.
const TEXT_MUTED: Color = Color::from_rgb(0.5, 0.5, 0.56);
/// Error / destructive text.
const TEXT_DANGER: Color = Color::from_rgb(0.9, 0.3, 0.3);
/// Success text (e.g. a passing connection test).
const TEXT_SUCCESS: Color = Color::from_rgb(0.13, 0.62, 0.33);
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use sshoal_core::updater::{self, RELEASES_URL, UpdateInfo};
use sshoal_core::{
    AppConfig, Backoff, ImportError, OpenSshTransport, PortableConfig, Settings, SshConfig,
    Transport, Tunnel, TunnelState, TunnelSupervisor, export_portable, import_portable,
};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

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

/// The open dropdown: a tunnel's row (acts on the selection) or a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContextMenu {
    Tunnel(usize),
    Folder(String),
}

/// What a pending delete-confirmation will remove.
#[derive(Debug, Clone)]
enum PendingDelete {
    /// Tunnels by `path`.
    Tunnels(Vec<String>),
    /// An SSH config by `name`.
    Ssh(String),
}

#[derive(Debug, Clone)]
enum Message {
    /// Periodic: refresh status dots and poll the tray menu channel.
    Tick,
    ClickFolder(String),
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    StartAdd,
    StartEdit(usize),
    EditField(Field, String),
    SaveEdit,
    CancelEdit,
    /// Run a one-shot connectivity test against the form's current values.
    TestConnection,
    /// Result of the test — Ok(()) (reachable) or a human-readable failure.
    TestConnectionResult(Result<(), String>),
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
    ToggleTunnel(usize),
    // Row interaction: click = select (never connect), ⌘/Shift-click = multi,
    // right-click = inline options dropdown.
    RowPress(usize),
    RowRightPress(usize),
    FolderPress(String),
    FolderMenu(String),
    ActivateSelected,
    Escape,
    Resized(f32),
    SelectDelta(i32),
    ModifiersChanged(iced::keyboard::Modifiers),
    CursorMoved(iced::Point),
    CloseContextMenu,
    // Tunnel dropdown options — act on the current selection.
    MenuEdit,
    MenuConnect,
    MenuDisconnect,
    MenuDelete,
    // Folder dropdown options — act on the folder's tunnels.
    FolderConnectAll,
    FolderDisconnectAll,
    FolderDelete,
    // Folder rename (popover with a new-name field).
    FolderRename,
    RenameInput(String),
    RenameConfirm,
    RenameCancel,
    // Delete confirmation (every delete is confirmed).
    ConfirmDelete,
    CancelDelete,
    OpenTerminal(usize),
    // Filter bar (tunnels screen)
    FilterInput(String),
    SetFilter(StateFilter),
    // Preferences screen + auto-update.
    OpenPrefs,
    ClosePrefs,
    ToggleAutoUpdate(bool),
    /// Run an update check now (from launch or the "Check now" button).
    CheckUpdates,
    /// Result of a check — Ok(info) or a human-readable failure.
    UpdateChecked(Result<UpdateInfo, String>),
    /// Open the release page (or the releases list) in the browser.
    OpenReleasePage,
    /// Open the GitHub "new issue" page in the browser.
    ReportBug,
    /// Hide the update banner and remember not to show this version again.
    DismissUpdate,
    /// Open the confirm-to-update popover.
    PromptUpdate,
    /// Close the update popover without doing anything.
    CloseUpdatePrompt,
    /// Download + install the latest release, then relaunch.
    InstallUpdate,
    /// Result of the install — Ok(()) (about to relaunch) or a failure.
    UpdateInstalled(Result<(), String>),
    // Backup: Export / Import (Preferences → Backup).
    /// Open the export-options popover.
    OpenExport,
    ExportToggleEncrypt(bool),
    ExportToggleKeys(bool),
    ExportPassphrase(String),
    /// Validate the form, then open the native save dialog.
    ExportPick,
    /// Save-dialog result; `Some(path)` → build + write the export there.
    ExportPicked(Option<PathBuf>),
    /// Open the native open dialog to import.
    OpenImport,
    /// Open-dialog result: the chosen file's bytes (`None` if cancelled).
    ImportPicked(Option<Vec<u8>>),
    ImportPassphraseInput(String),
    /// Decrypt + merge the picked file using the typed passphrase.
    ImportConfirm,
    /// Result of an export/import — a human-readable status line.
    BackupFinished(Result<String, String>),
    /// Close the Export/Import popover.
    BackupCancel,
}

struct MenuIds {
    connect_all: MenuId,
    open: MenuId,
    prefs: MenuId,
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
    /// A "Test connection" probe is in flight (disables the button).
    testing: bool,
    /// Result of the last probe: `Ok(())` = reachable, `Err(reason)` = failed.
    /// Cleared whenever a field changes (so a stale verdict never lingers).
    test_result: Option<Result<(), String>>,
}

#[derive(Debug, Clone, Copy)]
enum SshField {
    Name,
    Host,
    Port,
    User,
    Identity,
}

/// In-progress folder rename. `path` is the folder being renamed; `name` is the
/// new leaf segment the user is typing (e.g. renaming `gc/dev` edits `dev`).
#[derive(Default)]
struct RenameFolder {
    path: String,
    name: String,
    error: Option<String>,
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

/// The Export/Import popover (Preferences → Backup).
enum Backup {
    /// Export options + passphrase, before the save dialog.
    Export(ExportForm),
    /// A picked, encrypted file awaiting its passphrase to import.
    ImportPassphrase(ImportForm),
}

/// Options for an export (mirrors the CLI flags).
struct ExportForm {
    /// Encrypt the file (default on). Off = plaintext YAML.
    encrypt: bool,
    /// Embed private-key contents (forces `encrypt`).
    include_keys: bool,
    passphrase: String,
    error: Option<String>,
}

/// A decrypt-on-import prompt: the file's bytes + the passphrase being typed.
#[derive(Default)]
struct ImportForm {
    bytes: Vec<u8>,
    passphrase: String,
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
    /// Folder paths the user has collapsed (everything else is expanded). Mirrors
    /// `Settings::collapsed_folders` and is persisted, so collapse state survives
    /// connect/disconnect, save/delete and app restarts.
    collapsed: HashSet<String>,
    editing: Option<EditForm>,
    /// Showing the SSH-configs list screen.
    managing_ssh: bool,
    /// In-progress add/edit of an SSH config.
    editing_ssh: Option<SshForm>,
    /// Multi-selected tunnel paths (⌘/Shift-click) for bulk actions.
    checked: HashSet<String>,
    /// Fixed anchor for Shift range selection (click or arrow).
    select_anchor: Option<String>,
    /// Moving end of a Shift+arrow range (the row the cursor is on).
    select_cursor: Option<String>,
    /// Live keyboard modifiers (to interpret clicks).
    modifiers: iced::keyboard::Modifiers,
    /// The open options dropdown (tunnel selection or folder).
    context_menu: Option<ContextMenu>,
    /// Live cursor position (window coords) for placing the dropdown.
    cursor: iced::Point,
    /// Where the dropdown is anchored (cursor when it opened).
    menu_at: iced::Point,
    /// A delete awaiting confirmation (every delete is confirmed).
    confirm_delete: Option<PendingDelete>,
    /// An in-progress folder rename (the popover with the new-name field).
    renaming: Option<RenameFolder>,
    /// Free-text filter (matches tunnel name or folder path).
    filter: String,
    /// Connection-state filter.
    filter_state: StateFilter,
    /// Current window width (for fitting tunnel names with an ellipsis).
    window_width: f32,
    /// Persisted preferences (auto-update toggle, skipped version).
    settings: Settings,
    /// Showing the Preferences screen.
    managing_prefs: bool,
    /// An available, not-yet-dismissed update — drives the banner.
    update_info: Option<UpdateInfo>,
    /// A check is in flight (disables the "Check now" button).
    update_checking: bool,
    /// Transient result of the last manual check, shown in Preferences.
    update_status: Option<String>,
    /// The confirm-to-update popover is open.
    update_prompt: bool,
    /// An install (download + replace) is in flight.
    update_installing: bool,
    /// The Export/Import popover (Preferences → Backup), if open.
    backup: Option<Backup>,
    /// Transient result of the last export/import, shown in Preferences.
    backup_status: Option<String>,
}

impl App {
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
            &self.collapsed,
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
                    || r.tunnel.name().to_lowercase().contains(&q)
                    || r.tunnel.local_port.to_string().contains(&q);
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

    let settings = config.settings;
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
    // Restore the user's collapse state, dropping any saved path whose folder no
    // longer exists (so the set never accretes stale entries).
    let all_folders = all_folder_paths(&rows);
    let collapsed: HashSet<String> = settings
        .collapsed_folders
        .iter()
        .filter(|p| all_folders.contains(*p))
        .cloned()
        .collect();

    let mut app = App {
        tray: None,
        menu: None,
        _hotkey: None,
        window: None,
        runtime,
        transport: Arc::new(OpenSshTransport),
        ssh_configs,
        rows,
        collapsed,
        editing: None,
        managing_ssh: false,
        editing_ssh: None,
        checked: HashSet::new(),
        select_anchor: None,
        select_cursor: None,
        modifiers: iced::keyboard::Modifiers::default(),
        context_menu: None,
        cursor: iced::Point::ORIGIN,
        menu_at: iced::Point::ORIGIN,
        confirm_delete: None,
        renaming: None,
        filter: String::new(),
        filter_state: StateFilter::All,
        window_width: 360.0,
        settings,
        managing_prefs: false,
        update_info: None,
        update_checking: false,
        update_status: None,
        update_prompt: false,
        update_installing: false,
        backup: None,
        backup_status: None,
    };

    // Show the window on launch so opening the app always surfaces it (the tray
    // icon can be hard to spot); `gain_focus` brings it to the front since we
    // run as a menu-bar accessory with no Dock icon.
    let (id, open_task) = window::open(open_window_settings());
    app.window = Some(id);
    let mut tasks = vec![open_task.map(Message::WindowOpened), window::gain_focus(id)];
    // Best-effort update check on launch when enabled — read-only, never installs.
    if app.settings.auto_update_enabled {
        app.update_checking = true;
        tasks.push(check_update_task(app.runtime.clone()));
    }
    (app, Task::batch(tasks))
}

/// A background GitHub release check, off the UI thread (the `curl` call blocks).
fn check_update_task(runtime: Arc<tokio::runtime::Runtime>) -> Task<Message> {
    Task::perform(
        async move {
            match runtime
                .spawn_blocking(|| updater::check_latest(VERSION))
                .await
            {
                Ok(Ok(info)) => Ok(info),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            }
        },
        Message::UpdateChecked,
    )
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

            let ids = app.menu.as_ref().map(|m| {
                (
                    m.open.clone(),
                    m.quit.clone(),
                    m.connect_all.clone(),
                    m.prefs.clone(),
                )
            });
            let rx = MenuEvent::receiver();
            while let Ok(event) = rx.try_recv() {
                let Some((open, quit, connect_all, prefs)) = ids.as_ref() else {
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
                } else if event.id == *prefs {
                    // Surface the window (creating it if needed) on the Preferences
                    // screen, clearing any other screen/form so it lands clean.
                    app.managing_prefs = true;
                    app.managing_ssh = false;
                    app.editing = None;
                    app.editing_ssh = None;
                    app.context_menu = None;
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

            // Left-click the tray icon → toggle the popover window. (Right-click
            // still opens the menu; the menu's "Open" and ⌃⌘S remain as backups
            // for when the icon is hidden behind the notch.)
            let tray_rx = TrayIconEvent::receiver();
            while let Ok(event) = tray_rx.try_recv() {
                if matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    }
                ) {
                    if let Some(id) = app.window {
                        return window::close(id);
                    }
                    let (id, task) = window::open(open_window_settings());
                    app.window = Some(id);
                    return Task::batch([task.map(Message::WindowOpened), window::gain_focus(id)]);
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
        Message::ClickFolder(path) => {
            app.context_menu = None;
            // Toggle: if it was collapsed, expand it; otherwise collapse it.
            if !app.collapsed.remove(&path) {
                app.collapsed.insert(path);
            }
            persist_app(app);
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
            app.context_menu = None;
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
                    ..EditForm::default()
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
                // The values changed — any earlier test verdict is now stale.
                form.test_result = None;
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
        Message::TestConnection => {
            // Probe the in-form values without saving. Needs a valid ssh target,
            // remote host and remote port; the local port is irrelevant (the test
            // uses a throwaway one).
            let Some((tunnel, ssh)) = form_test_target(app) else {
                return Task::none();
            };
            match &mut app.editing {
                Some(form) if form.testing => return Task::none(),
                Some(form) => {
                    form.testing = true;
                    form.test_result = None;
                }
                None => return Task::none(),
            }
            let runtime = app.runtime.clone();
            let transport = app.transport.clone();
            Task::perform(
                async move {
                    runtime
                        .spawn(async move { transport.test(&tunnel, &ssh).await })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                },
                Message::TestConnectionResult,
            )
        }
        Message::TestConnectionResult(result) => {
            if let Some(form) = &mut app.editing {
                form.testing = false;
                form.test_result = Some(result);
            }
            Task::none()
        }
        Message::DeleteTunnel(i) => {
            // Ask first — every delete is confirmed.
            if let Some(row) = app.rows.get(i) {
                app.editing = None;
                app.confirm_delete = Some(PendingDelete::Tunnels(vec![row.tunnel.path.clone()]));
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
            if let Some(c) = app.ssh_configs.get(i) {
                app.editing_ssh = None;
                app.confirm_delete = Some(PendingDelete::Ssh(c.name.clone()));
            }
            Task::none()
        }
        Message::ModifiersChanged(m) => {
            app.modifiers = m;
            Task::none()
        }
        Message::ToggleTunnel(i) => {
            let on = !app.rows.get(i).map(TunnelRow::enabled).unwrap_or(false);
            set_enabled(app, i, on);
            Task::none()
        }
        Message::RowPress(i) => {
            // Click SELECTS (never connects). ⌘ toggles one in/out; Shift extends
            // a range; a plain click selects just this row.
            // Left-click = select (plain/⌘/Shift). Edit is via the menu or Enter.
            if let Some(path) = app.rows.get(i).map(|r| r.tunnel.path.clone()) {
                apply_select(app, path);
            }
            Task::none()
        }
        Message::RowRightPress(i) => {
            // Open the options dropdown at the cursor. If this row isn't already
            // selected, select just it first so the menu acts on it.
            if let Some(path) = app.rows.get(i).map(|r| r.tunnel.path.clone()) {
                if !app.checked.contains(&path) {
                    app.checked.clear();
                    app.checked.insert(path.clone());
                    app.select_anchor = Some(path);
                }
                app.menu_at = app.cursor;
                app.context_menu = Some(ContextMenu::Tunnel(i));
            }
            Task::none()
        }
        Message::FolderPress(path) => {
            // Plain left-click anywhere on a folder row toggles it open/closed —
            // the small glyph was too easy to miss. ⌘/Shift-click still selects
            // for bulk actions; right-click still opens the folder menu.
            if app.modifiers.command() || app.modifiers.shift() {
                apply_select(app, path);
                Task::none()
            } else {
                update(app, Message::ClickFolder(path))
            }
        }
        Message::FolderMenu(path) => {
            // Right-click a folder → open its dropdown at the cursor.
            if !app.checked.contains(&path) {
                app.checked.clear();
                app.checked.insert(path.clone());
                app.select_anchor = Some(path.clone());
                app.select_cursor = Some(path.clone());
            }
            app.menu_at = app.cursor;
            app.context_menu = Some(ContextMenu::Folder(path));
            Task::none()
        }
        Message::Escape => {
            // Back out one level; if nothing is open, hide the window (the app
            // keeps running in the tray — reopen with ⌃⌘S or the tray icon).
            if app.confirm_delete.take().is_some()
                || app.backup.take().is_some()
                || app.renaming.take().is_some()
                || app.context_menu.take().is_some()
                || app.editing.take().is_some()
                || app.editing_ssh.take().is_some()
            {
                return Task::none();
            }
            if app.managing_ssh {
                app.managing_ssh = false;
                return Task::none();
            }
            if app.managing_prefs {
                app.managing_prefs = false;
                return Task::none();
            }
            if let Some(id) = app.window {
                return window::close(id);
            }
            Task::none()
        }
        Message::ActivateSelected => {
            // Enter on the selection: edit a single tunnel, or toggle a single
            // folder. Ignored while a form/menu is up.
            if app.editing.is_some()
                || app.editing_ssh.is_some()
                || app.confirm_delete.is_some()
                || app.renaming.is_some()
                || app.managing_ssh
                || app.context_menu.is_some()
            {
                return Task::none();
            }
            if app.checked.len() == 1 {
                let p = app.checked.iter().next().cloned().unwrap();
                if let Some(i) = app.rows.iter().position(|r| r.tunnel.path == p) {
                    return update(app, Message::StartEdit(i));
                }
                return update(app, Message::ClickFolder(p));
            }
            Task::none()
        }
        Message::SelectDelta(delta) => {
            // Arrow-key navigation: move the single selection through the visible
            // tunnels. Ignored while a form/menu is up.
            if app.editing.is_some()
                || app.editing_ssh.is_some()
                || app.confirm_delete.is_some()
                || app.renaming.is_some()
                || app.managing_ssh
            {
                return Task::none();
            }
            // Walk ALL visible rows — folders and tunnels alike.
            let order: Vec<(String, bool)> = app
                .display_rows()
                .into_iter()
                .map(|d| (d.path, d.row_idx.is_some()))
                .collect();
            if order.is_empty() {
                return Task::none();
            }
            let pos = |p: &Option<String>| {
                p.as_ref()
                    .and_then(|x| order.iter().position(|(q, _)| q == x))
            };
            // The moving end starts from the current cursor (or anchor).
            let end = pos(&app.select_cursor).or_else(|| pos(&app.select_anchor));
            let next = match end {
                Some(e) => (e as i32 + delta).clamp(0, order.len() as i32 - 1) as usize,
                None if delta > 0 => 0,
                None => order.len() - 1,
            };

            if app.modifiers.shift() {
                // Extend: select every row (folder or tunnel) between the anchor
                // and the new end.
                let anchor = pos(&app.select_anchor).unwrap_or(next);
                if app.select_anchor.is_none() {
                    app.select_anchor = Some(order[anchor].0.clone());
                }
                let (lo, hi) = (anchor.min(next), anchor.max(next));
                app.checked.clear();
                for (p, _) in order.iter().take(hi + 1).skip(lo) {
                    app.checked.insert(p.clone());
                }
                app.select_cursor = Some(order[next].0.clone());
            } else {
                // Plain move: clear everything, select just this row.
                let path = order[next].0.clone();
                app.checked.clear();
                app.checked.insert(path.clone());
                app.select_anchor = Some(path.clone());
                app.select_cursor = Some(path);
            }
            Task::none()
        }
        Message::CursorMoved(p) => {
            app.cursor = p;
            Task::none()
        }
        Message::Resized(w) => {
            app.window_width = w;
            Task::none()
        }
        Message::CloseContextMenu => {
            app.context_menu = None;
            Task::none()
        }
        Message::MenuEdit => {
            app.context_menu = None;
            let idxs = checked_indices(app);
            if let [i] = idxs[..] {
                return update(app, Message::StartEdit(i));
            }
            Task::none()
        }
        Message::MenuConnect => {
            app.context_menu = None;
            for i in checked_indices(app) {
                set_enabled(app, i, true);
            }
            Task::none()
        }
        Message::MenuDisconnect => {
            app.context_menu = None;
            for i in checked_indices(app) {
                set_enabled(app, i, false);
            }
            Task::none()
        }
        Message::MenuDelete => {
            app.context_menu = None;
            let paths: Vec<String> = checked_indices(app)
                .into_iter()
                .map(|i| app.rows[i].tunnel.path.clone())
                .collect();
            if !paths.is_empty() {
                app.confirm_delete = Some(PendingDelete::Tunnels(paths));
            }
            Task::none()
        }
        Message::FolderConnectAll => {
            let folder = context_folder(app);
            app.context_menu = None;
            if let Some(path) = folder {
                for i in descendant_indices(&app.rows, &path) {
                    set_enabled(app, i, true);
                }
            }
            Task::none()
        }
        Message::FolderDisconnectAll => {
            let folder = context_folder(app);
            app.context_menu = None;
            if let Some(path) = folder {
                for i in descendant_indices(&app.rows, &path) {
                    set_enabled(app, i, false);
                }
            }
            Task::none()
        }
        Message::FolderDelete => {
            let folder = context_folder(app);
            app.context_menu = None;
            if let Some(path) = folder {
                let paths: Vec<String> = descendant_indices(&app.rows, &path)
                    .into_iter()
                    .map(|i| app.rows[i].tunnel.path.clone())
                    .collect();
                if !paths.is_empty() {
                    app.confirm_delete = Some(PendingDelete::Tunnels(paths));
                }
            }
            Task::none()
        }
        Message::FolderRename => {
            let folder = context_folder(app);
            app.context_menu = None;
            if let Some(path) = folder {
                let name = path.rsplit('/').next().unwrap_or(&path).to_string();
                app.renaming = Some(RenameFolder {
                    path,
                    name,
                    error: None,
                });
                // Put the cursor in the field so the user can type immediately.
                return iced::widget::operation::focus(RENAME_ID);
            }
            Task::none()
        }
        Message::RenameInput(value) => {
            if let Some(form) = &mut app.renaming {
                form.name = value;
                form.error = None;
            }
            Task::none()
        }
        Message::RenameConfirm => {
            rename_folder(app);
            Task::none()
        }
        Message::RenameCancel => {
            app.renaming = None;
            Task::none()
        }
        Message::CancelDelete => {
            app.confirm_delete = None;
            Task::none()
        }
        Message::ConfirmDelete => {
            match app.confirm_delete.take() {
                Some(PendingDelete::Tunnels(paths)) => {
                    let doomed: HashSet<String> = paths.into_iter().collect();
                    for row in &mut app.rows {
                        if doomed.contains(&row.tunnel.path)
                            && let Some(sup) = row.supervisor.take()
                        {
                            sup.cancel();
                        }
                    }
                    app.rows.retain(|r| !doomed.contains(&r.tunnel.path));
                    for p in &doomed {
                        app.checked.remove(p);
                    }
                    info!(count = doomed.len(), "delete tunnels");
                    reconcile_collapsed(app);
                    persist_app(app);
                }
                Some(PendingDelete::Ssh(name)) => {
                    app.ssh_configs.retain(|c| c.name != name);
                    info!(ssh = %name, "delete ssh config");
                    persist_app(app);
                }
                None => {}
            }
            Task::none()
        }
        Message::OpenTerminal(i) => {
            app.context_menu = None;
            if let Some(row) = app.rows.get(i) {
                let ssh = app.resolve_ssh(&row.tunnel);
                open_terminal(&ssh);
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
        Message::OpenPrefs => {
            // A clean top-level screen: drop any other screen/form first.
            app.managing_prefs = true;
            app.managing_ssh = false;
            app.editing = None;
            app.editing_ssh = None;
            app.context_menu = None;
            Task::none()
        }
        Message::ClosePrefs => {
            app.managing_prefs = false;
            Task::none()
        }
        Message::ToggleAutoUpdate(on) => {
            app.settings.auto_update_enabled = on;
            persist_app(app);
            // Flipping it on with no result yet: check straight away.
            if on && app.update_info.is_none() && !app.update_checking {
                app.update_checking = true;
                app.update_status = None;
                return check_update_task(app.runtime.clone());
            }
            Task::none()
        }
        Message::CheckUpdates => {
            if app.update_checking {
                return Task::none();
            }
            app.update_checking = true;
            app.update_status = None;
            check_update_task(app.runtime.clone())
        }
        Message::UpdateChecked(result) => {
            app.update_checking = false;
            match result {
                Ok(info) if info.available => {
                    // Honour a version the user chose to dismiss.
                    if app.settings.skipped_version.as_deref() == Some(info.latest.as_str()) {
                        app.update_info = None;
                        app.update_status = Some(format!("{} available (dismissed)", info.latest));
                    } else {
                        app.update_status = Some(format!("Update available: {}", info.latest));
                        app.update_info = Some(info);
                    }
                }
                Ok(info) => {
                    app.update_info = None;
                    app.update_status = Some(format!("Up to date (v{})", info.current));
                }
                Err(e) => app.update_status = Some(format!("Check failed: {e}")),
            }
            Task::none()
        }
        Message::OpenReleasePage => {
            let url = app
                .update_info
                .as_ref()
                .map(|i| i.url.clone())
                .unwrap_or_else(|| RELEASES_URL.to_string());
            open_url(&url);
            Task::none()
        }
        Message::ReportBug => {
            open_url(ISSUES_URL);
            Task::none()
        }
        Message::DismissUpdate => {
            if let Some(info) = app.update_info.take() {
                app.settings.skipped_version = Some(info.latest);
                persist_app(app);
            }
            Task::none()
        }
        Message::PromptUpdate => {
            if app.update_info.is_some() {
                app.update_prompt = true;
            }
            Task::none()
        }
        Message::CloseUpdatePrompt => {
            if !app.update_installing {
                app.update_prompt = false;
            }
            Task::none()
        }
        Message::InstallUpdate => {
            if app.update_installing {
                return Task::none();
            }
            app.update_installing = true;
            app.update_status = Some("Downloading and installing…".to_string());
            let runtime = app.runtime.clone();
            Task::perform(
                async move {
                    runtime
                        .spawn_blocking(updater::install_latest)
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                },
                Message::UpdateInstalled,
            )
        }
        Message::UpdateInstalled(result) => {
            app.update_installing = false;
            match result {
                Ok(()) => {
                    info!("update installed; relaunching");
                    relaunch_app();
                    return iced::exit();
                }
                Err(e) => {
                    app.update_status = Some(format!("Update failed: {e}"));
                }
            }
            Task::none()
        }
        Message::OpenExport => {
            app.backup = Some(Backup::Export(ExportForm {
                encrypt: true,
                include_keys: false,
                passphrase: String::new(),
                error: None,
            }));
            app.backup_status = None;
            Task::none()
        }
        Message::ExportToggleEncrypt(on) => {
            if let Some(Backup::Export(f)) = &mut app.backup {
                f.encrypt = on;
                f.error = None;
            }
            Task::none()
        }
        Message::ExportToggleKeys(on) => {
            if let Some(Backup::Export(f)) = &mut app.backup {
                f.include_keys = on;
                f.error = None;
            }
            Task::none()
        }
        Message::ExportPassphrase(s) => {
            if let Some(Backup::Export(f)) = &mut app.backup {
                f.passphrase = s;
                f.error = None;
            }
            Task::none()
        }
        Message::ExportPick => {
            let Some(Backup::Export(f)) = &mut app.backup else {
                return Task::none();
            };
            if f.encrypt && f.passphrase.chars().count() < cli::MIN_PASSPHRASE_LEN {
                f.error = Some(format!(
                    "Passphrase too short (min {} chars)",
                    cli::MIN_PASSPHRASE_LEN
                ));
                return Task::none();
            }
            // Timestamp the default name with the Unix epoch (seconds) so exports
            // don't overwrite each other and still sort chronologically, e.g.
            // sshoal_1782561000.age.
            let stamp = chrono::Local::now().timestamp();
            let ext = if f.encrypt { "age" } else { "yaml" };
            let name = format!("sshoal_{stamp}.{ext}");
            Task::perform(
                async move {
                    rfd::AsyncFileDialog::new()
                        .set_file_name(name)
                        .save_file()
                        .await
                        .map(|h| h.path().to_path_buf())
                },
                Message::ExportPicked,
            )
        }
        Message::ExportPicked(path) => {
            let Some(path) = path else {
                return Task::none(); // cancelled — leave the popover open
            };
            let Some(Backup::Export(f)) = &app.backup else {
                return Task::none();
            };
            let passphrase = f.encrypt.then(|| f.passphrase.clone());
            let include_keys = f.include_keys;
            let cfg = AppConfig {
                ssh_configs: app.ssh_configs.clone(),
                tunnels: app.rows.iter().map(|r| r.tunnel.clone()).collect(),
                settings: Settings::default(),
            };
            let portable = PortableConfig::build(&cfg, None, false);
            app.backup = None;
            let runtime = app.runtime.clone();
            Task::perform(
                async move {
                    runtime
                        .spawn_blocking(move || {
                            do_export(portable, include_keys, passphrase, &path)
                        })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                },
                Message::BackupFinished,
            )
        }
        Message::OpenImport => {
            app.backup_status = None;
            Task::perform(
                async move {
                    match rfd::AsyncFileDialog::new().pick_file().await {
                        Some(h) => Some(h.read().await),
                        None => None,
                    }
                },
                Message::ImportPicked,
            )
        }
        Message::ImportPicked(bytes) => {
            let Some(bytes) = bytes else {
                return Task::none(); // cancelled
            };
            // Plaintext imports apply at once; encrypted ones need a passphrase.
            match import_portable(&bytes, None) {
                Ok(portable) => apply_import(app, portable),
                Err(ImportError::PassphraseRequired) => {
                    app.backup = Some(Backup::ImportPassphrase(ImportForm {
                        bytes,
                        ..Default::default()
                    }));
                    Task::none()
                }
                Err(e) => {
                    app.backup_status = Some(format!("Import failed: {e}"));
                    Task::none()
                }
            }
        }
        Message::ImportPassphraseInput(s) => {
            if let Some(Backup::ImportPassphrase(f)) = &mut app.backup {
                f.passphrase = s;
                f.error = None;
            }
            Task::none()
        }
        Message::ImportConfirm => {
            let Some(Backup::ImportPassphrase(f)) = &app.backup else {
                return Task::none();
            };
            let bytes = f.bytes.clone();
            let pass = f.passphrase.clone();
            match import_portable(&bytes, Some(&pass)) {
                Ok(portable) => {
                    app.backup = None;
                    apply_import(app, portable)
                }
                Err(e) => {
                    if let Some(Backup::ImportPassphrase(f)) = &mut app.backup {
                        f.error = Some(format!("{e}"));
                    }
                    Task::none()
                }
            }
        }
        Message::BackupFinished(result) => {
            app.backup_status = Some(match result {
                Ok(s) => s,
                Err(e) => format!("Failed: {e}"),
            });
            Task::none()
        }
        Message::BackupCancel => {
            app.backup = None;
            Task::none()
        }
        Message::WindowOpened(id) => {
            info!(window = ?id, "window opened");
            // Put the cursor in the search box so you can filter by just typing.
            iced::widget::operation::focus(FILTER_ID)
        }
        Message::WindowClosed(id) => {
            // Forget the window so the next "Open" can spawn a fresh one, and
            // drop any selection so reopening starts clean.
            if app.window == Some(id) {
                app.window = None;
                app.checked.clear();
                app.select_anchor = None;
                app.select_cursor = None;
                app.context_menu = None;
            }
            Task::none()
        }
    }
}

/// Widget id for the filter search box, so we can focus it when the window
/// opens (otherwise typing goes nowhere until you click into it).
const FILTER_ID: &str = "filter";

/// Widget id for the rename-folder field, focused when the popover opens.
const RENAME_ID: &str = "rename-folder";

/// Row indices the selection resolves to: tunnels checked directly, plus every
/// tunnel under a checked folder path.
fn checked_indices(app: &App) -> Vec<usize> {
    app.rows
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            let p = &r.tunnel.path;
            app.checked.contains(p) || app.checked.iter().any(|c| p.starts_with(&format!("{c}/")))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Apply a click selection (⌘ toggles, Shift extends a range, plain replaces)
/// for `path` — works for both folder and tunnel paths.
fn apply_select(app: &mut App, path: String) {
    app.context_menu = None;
    if app.modifiers.command() {
        if !app.checked.remove(&path) {
            app.checked.insert(path.clone());
        }
        app.select_anchor = Some(path.clone());
    } else if app.modifiers.shift() {
        let order: Vec<String> = app.display_rows().into_iter().map(|d| d.path).collect();
        let cur = order.iter().position(|p| p == &path);
        let anchor = app
            .select_anchor
            .as_ref()
            .and_then(|a| order.iter().position(|p| p == a));
        match (anchor, cur) {
            (Some(a), Some(c)) => {
                for p in &order[a.min(c)..=a.max(c)] {
                    app.checked.insert(p.clone());
                }
            }
            _ => {
                app.checked.insert(path.clone());
                app.select_anchor = Some(path.clone());
            }
        }
    } else {
        app.checked.clear();
        app.checked.insert(path.clone());
        app.select_anchor = Some(path.clone());
    }
    app.select_cursor = Some(path);
}

/// The folder path of the open folder dropdown, if any.
fn context_folder(app: &App) -> Option<String> {
    match &app.context_menu {
        Some(ContextMenu::Folder(p)) => Some(p.clone()),
        _ => None,
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

/// Open a terminal with an interactive SSH session to a tunnel's host. macOS
/// uses Terminal.app via AppleScript; Linux tries the common emulators.
fn open_terminal(ssh: &SshConfig) {
    let cmd = ssh_login_command(ssh);
    info!(host = %ssh.host, "open terminal");
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Terminal\"\n    do script \"{}\"\n    activate\nend tell",
            cmd.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        for term in ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"] {
            let ok = std::process::Command::new(term)
                .args(["-e", "sh", "-c", &format!("{cmd}; exec $SHELL")])
                .spawn()
                .is_ok();
            if ok {
                break;
            }
        }
    }
}

/// Build the interactive `ssh user@host` command (no port-forward flags) for a
/// terminal session.
fn ssh_login_command(ssh: &SshConfig) -> String {
    let mut parts = vec!["ssh".to_string()];
    if let Some(id) = &ssh.identity_file {
        parts.push("-i".into());
        parts.push(id.clone());
    }
    if ssh.port != 22 {
        parts.push("-p".into());
        parts.push(ssh.port.to_string());
    }
    parts.push(match &ssh.user {
        Some(u) => format!("{u}@{}", ssh.host),
        None => ssh.host.clone(),
    });
    parts.join(" ")
}

/// Build the (tunnel, ssh) pair a "Test connection" probe should run against
/// from the current edit form, or `None` if the form isn't testable yet. The
/// `path` and `local_port` don't affect the probe (it uses a throwaway port), so
/// only the ssh target, remote host and remote port need to be valid.
fn form_test_target(app: &App) -> Option<(Tunnel, SshConfig)> {
    let form = app.editing.as_ref()?;
    let ssh_name = form.ssh.trim();
    let remote_host = form.remote_host.trim();
    if ssh_name.is_empty() || remote_host.is_empty() {
        return None;
    }
    let remote_port = form.remote_port.trim().parse::<u16>().ok()?;
    let tunnel = Tunnel {
        path: "test".to_string(),
        ssh: ssh_name.to_string(),
        local_port: form.local_port.trim().parse::<u16>().unwrap_or(0),
        remote_host: remote_host.to_string(),
        remote_port,
    };
    let ssh = app.resolve_ssh(&tunnel);
    Some((tunnel, ssh))
}

/// Whether the edit form has enough to run a "Test connection" probe (mirrors
/// the checks in [`form_test_target`], for enabling/disabling the button).
fn test_ready(form: &EditForm) -> bool {
    !form.ssh.trim().is_empty()
        && !form.remote_host.trim().is_empty()
        && form.remote_port.trim().parse::<u16>().is_ok()
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
    reconcile_collapsed(app);
    persist_app(app);
}

/// Drop collapse entries for folders that no longer exist after a row change.
/// We never *add* entries here, so a folder the user didn't collapse — including
/// a newly appeared one — stays expanded; only their explicit choices persist.
fn reconcile_collapsed(app: &mut App) {
    let all = all_folder_paths(&app.rows);
    app.collapsed.retain(|p| all.contains(p));
}

/// Write the current ssh configs + tunnels + settings (with the live collapse
/// state folded in) back to the config file.
fn persist_app(app: &App) {
    let mut settings = app.settings.clone();
    let mut collapsed_folders: Vec<String> = app.collapsed.iter().cloned().collect();
    collapsed_folders.sort(); // stable YAML output
    settings.collapsed_folders = collapsed_folders;
    let config = AppConfig {
        ssh_configs: app.ssh_configs.clone(),
        tunnels: app.rows.iter().map(|r| r.tunnel.clone()).collect(),
        settings,
    };
    if let Err(e) = config.save(config_path()) {
        tracing::error!(error = %e, "failed to save config");
    }
}

/// Build (optionally embedding keys), encrypt-or-not, and write an export. Runs
/// off the UI thread (Argon2id + file I/O). Returns a status line.
fn do_export(
    mut portable: PortableConfig,
    include_keys: bool,
    passphrase: Option<String>,
    path: &Path,
) -> Result<String, String> {
    if include_keys {
        portable.keys = cli::gather_keys(&portable);
    }
    let blob = export_portable(&portable, passphrase.as_deref()).map_err(|e| e.to_string())?;
    std::fs::write(path, &blob).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!(
        "Exported {} tunnel(s) to {}",
        portable.tunnels.len(),
        path.display()
    ))
}

/// Merge an imported config into the running app: materialize embedded keys, add
/// new ssh configs + tunnels (skipping conflicts, so running tunnels and existing
/// entries are untouched), reconcile collapse state, and persist.
fn apply_import(app: &mut App, mut portable: PortableConfig) -> Task<Message> {
    let wrote = cli::materialize_keys(&mut portable, false);
    for c in portable.ssh_configs {
        if !app.ssh_configs.iter().any(|x| x.name == c.name) {
            app.ssh_configs.push(c);
        }
    }
    let mut added = 0;
    for t in portable.tunnels {
        if app.rows.iter().any(|r| r.tunnel.path == t.path) {
            continue; // keep the existing tunnel (and its running supervisor)
        }
        app.rows.push(TunnelRow {
            tunnel: t,
            supervisor: None,
            status: TunnelState::Idle,
            notice: None,
            err_seen: None,
        });
        added += 1;
    }
    reconcile_collapsed(app);
    persist_app(app);
    let keys = if wrote > 0 {
        format!(", wrote {wrote} key file(s)")
    } else {
        String::new()
    };
    app.backup_status = Some(format!("Imported {added} new tunnel(s){keys}"));
    Task::none()
}

/// Apply a folder rename from the popover: rewrite the matching path prefix on
/// every descendant tunnel, then the collapse state, then persist. Live tunnels
/// keep running — only the `path` label changes, which the supervisor (holding
/// its own clone and keying ssh args off host/port, not `path`) never reads.
fn rename_folder(app: &mut App) {
    let Some(form) = &app.renaming else { return };
    let folder_path = form.path.clone();
    let new_leaf = form.name.trim().to_string();

    let invalid = if new_leaf.is_empty() {
        Some("Name is required".to_string())
    } else if new_leaf.contains('/') {
        Some("Name can't contain “/”".to_string())
    } else {
        None
    };
    if let Some(msg) = invalid {
        if let Some(form) = &mut app.renaming {
            form.error = Some(msg);
        }
        return;
    }

    // Swap the folder's last segment for the new name.
    let new_folder_path = match folder_path.rsplit_once('/') {
        Some((parent, _leaf)) => format!("{parent}/{new_leaf}"),
        None => new_leaf.clone(),
    };
    if new_folder_path == folder_path {
        app.renaming = None; // no-op rename
        return;
    }

    let old_prefix = format!("{folder_path}/");
    let rewrite = |p: &str| match p.strip_prefix(&old_prefix) {
        Some(rest) => format!("{new_folder_path}/{rest}"),
        None => p.to_string(),
    };

    // Compute every tunnel's resulting path and reject a rename that would make
    // two tunnels collide (tunnels are keyed by `path`).
    let new_paths: Vec<String> = app.rows.iter().map(|r| rewrite(&r.tunnel.path)).collect();
    let mut seen = HashSet::new();
    if let Some(dup) = new_paths.iter().find(|p| !seen.insert((*p).clone())) {
        if let Some(form) = &mut app.renaming {
            form.error = Some(format!("A tunnel already exists at “{dup}”"));
        }
        return;
    }

    // Apply — supervisors are left untouched, so connected tunnels don't flap.
    for (row, np) in app.rows.iter_mut().zip(new_paths) {
        row.tunnel.path = np;
    }
    // Carry the subtree's collapse state across the rename — the folder itself
    // (exact match) plus any collapsed descendant folders (prefix match).
    app.collapsed = app
        .collapsed
        .iter()
        .map(|p| {
            if p == &folder_path {
                new_folder_path.clone()
            } else {
                rewrite(p)
            }
        })
        .collect();
    // Selection paths are now stale.
    app.checked.clear();
    app.select_anchor = None;
    app.select_cursor = None;

    info!(from = %folder_path, to = %new_folder_path, "rename folder");
    app.renaming = None;
    persist_app(app);
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

/// Sort priority for environment-like folder names (lower sorts first).
/// Everything else ranks last and falls back to alphabetical.
fn env_rank(name: &str) -> u8 {
    match name.to_ascii_lowercase().as_str() {
        "dev" | "develop" | "development" => 0,
        "stg" | "stag" | "stage" | "staging" => 1,
        "prod" | "pro" | "prd" | "production" => 2,
        _ => 3,
    }
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
    collapsed: &HashSet<String>,
    allowed: Option<&HashSet<usize>>,
    out: &mut Vec<DisplayRow>,
) {
    // Order subfolders by environment (dev → staging → prod) then name, so the
    // tree reads dev-first regardless of alphabetical order.
    let mut subs: Vec<(&String, &Folder)> = folder.subfolders.iter().collect();
    subs.sort_by(|(a, _), (b, _)| env_rank(a).cmp(&env_rank(b)).then_with(|| a.cmp(b)));
    for (name, sub) in subs {
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
        // A folder is open unless the user collapsed it — honoured even while
        // filtering, so clicking a folder still expands/collapses it (the folder
        // row still shows because it has a matching descendant; collapsing just
        // hides the matches under it until it's expanded again).
        if !collapsed.contains(&path) {
            flatten(sub, &path, depth + 1, rows, collapsed, allowed, out);
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
    if let Some(form) = &app.editing_ssh {
        return ssh_edit_view(form);
    }
    if let Some(form) = &app.editing {
        return edit_view(form, &app.ssh_names());
    }

    // The base screen: Preferences, SSH-config list, or the tunnel tree.
    let base: Element<Message> = if app.managing_prefs {
        prefs_view(app)
    } else if app.managing_ssh {
        ssh_list_view(app)
    } else {
        tunnels_base(app)
    };

    // Delete confirmation floats centered over whatever screen is showing.
    if let Some(pending) = &app.confirm_delete {
        let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::CancelDelete);
        let centered = container(confirm_view(pending))
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        return stack![base, backdrop, centered].into();
    }
    // Rename-folder popover, centered over whatever screen is showing.
    if let Some(form) = &app.renaming {
        let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::RenameCancel);
        let centered = container(rename_view(form))
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        return stack![base, backdrop, centered].into();
    }
    // Export/Import popover, centered over whatever screen is showing.
    if let Some(backup) = &app.backup {
        let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::BackupCancel);
        let view = match backup {
            Backup::Export(form) => export_view(form),
            Backup::ImportPassphrase(form) => import_passphrase_view(form),
        };
        let centered = container(view)
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        return stack![base, backdrop, centered].into();
    }
    // Confirm-to-update popover, centered over whatever screen is showing.
    if app.update_prompt
        && let Some(info) = &app.update_info
    {
        let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseUpdatePrompt);
        let centered = container(update_prompt_view(info, app.update_installing))
            .center_x(Length::Fill)
            .center_y(Length::Fill);
        return stack![base, backdrop, centered].into();
    }
    // The options dropdown floats at the cursor (tunnels screen only).
    if !app.managing_ssh
        && !app.managing_prefs
        && let Some(menu) = &app.context_menu
    {
        let x = app.menu_at.x.clamp(0.0, 200.0);
        let y = app.menu_at.y.clamp(0.0, 480.0);
        let backdrop = mouse_area(space().width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseContextMenu);
        let floating = column![
            space().height(Length::Fixed(y)),
            row![space().width(Length::Fixed(x)), menu_panel(app, menu)],
        ];
        return stack![base, backdrop, floating].into();
    }
    // Keep the root a `stack` even with nothing floating: iced reconciles widget
    // state positionally, so if the root tag flipped Container↔Stack when a popup
    // opened, the list's scrollable would be rebuilt and snap back to the top.
    // Pinning `base` at child[0] preserves its scroll offset across every popup.
    stack![base].into()
}

fn tunnels_base(app: &App) -> Element<'_, Message> {
    // Compact header: search box (with a leading magnifier) + settings + add.
    let search = text_input("Search name, folder or port…", &app.filter)
        .id(FILTER_ID)
        .size(13)
        .padding([6, 9])
        .style(rounded_input)
        .icon(text_input::Icon {
            font: LUCIDE,
            code_point: ICON_SEARCH,
            size: Some(13.0.into()),
            spacing: 6.0,
            side: text_input::Side::Left,
        })
        .on_input(Message::FilterInput)
        .width(Length::Fill);
    let header = row![
        search,
        tip(
            icon_button(ICON_SLIDERS, 18.0, Message::OpenPrefs),
            "Preferences"
        ),
        tip(
            icon_button(ICON_SETTINGS, 18.0, Message::OpenSshConfigs),
            "SSH configs"
        ),
        tip(
            icon_button(ICON_PLUS, 18.0, Message::StartAdd),
            "Add tunnel"
        ),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);

    // State chips.
    let chip = |label: &'static str, state: StateFilter| {
        let active = app.filter_state == state;
        button(text(label).size(11))
            .style(move |_t: &iced::Theme, status| chip_style(active, status))
            .padding([3, 10])
            .on_press(Message::SetFilter(state))
    };
    let chips = row![
        chip("All", StateFilter::All),
        chip("Connected", StateFilter::Connected),
        chip("Disconnected", StateFilter::Disconnected),
    ]
    .spacing(6);

    let display = app.display_rows();
    let mut list = column![].spacing(1);
    if app.rows.is_empty() {
        list = list.push(text("No tunnels yet. Click + Add, or run `sshoal import-ssh`.").size(13));
    } else if display.is_empty() {
        list = list.push(
            text("No tunnels match the filter.")
                .size(13)
                .color(Color::from_rgb(0.5, 0.5, 0.56)),
        );
    }
    for (i, d) in display.iter().enumerate() {
        // A line between sibling folder groups: before a folder whose previous
        // row isn't its own parent (i.e. we just finished a sibling's subtree).
        if d.row_idx.is_none() && i != 0 && display[i - 1].depth >= d.depth {
            list = list.push(
                container(iced::widget::rule::horizontal(1).style(rule_style)).padding([3, 4]),
            );
        }
        list = list.push(tree_row(app, d));
    }

    // The scrollable runs nearly to the window's right edge (so the scrollbar
    // sits close to the edge), while the list content keeps a wider right inset
    // so the toggles stay well clear of the scrollbar. The non-scrolling
    // sections get their own right padding to line up.
    let body = scrollable(container(list).padding(pad_r(16.0)))
        .direction(thin_scrollbar())
        .style(scroll_style)
        .height(Length::Fill);
    // Keep `body` (the scrollable) at a fixed position in the screen tree so it
    // never gets rebuilt — see the note in `view`. The variable header bits
    // (search, the optional update banner, the chips) live in their own column,
    // so toggling the banner reshuffles only that sub-column and the list holds
    // its scroll offset.
    let mut top = column![container(header).padding(pad_r(10.0))].spacing(8);
    if let Some(info) = &app.update_info {
        top = top.push(container(update_banner(info)).padding(pad_r(10.0)));
    }
    if !app.rows.is_empty() {
        top = top.push(container(chips).padding(pad_r(10.0)));
    }
    let screen = column![top, body].spacing(8);
    container(screen)
        .padding(iced::Padding {
            top: 12.0,
            right: 2.0,
            bottom: 12.0,
            left: 12.0,
        })
        .into()
}

/// The "update available" strip shown atop the tunnel list. `View` opens the
/// release page; the `×` dismisses this version (it won't reappear until a newer
/// one ships). Installing is left to the user — sshoal never self-installs.
fn update_banner<'a>(info: &UpdateInfo) -> Element<'a, Message> {
    let label = text(format!(
        "Update available · v{} → {}",
        info.current, info.latest
    ))
    .size(12);
    let update = button(text("Update").size(12))
        .style(pill_button)
        .padding([3, 12])
        .on_press(Message::PromptUpdate);
    let dismiss = button(text("✕").size(12))
        .style(pill_secondary)
        .padding([3, 9])
        .on_press(Message::DismissUpdate);
    let bar = row![label, space().width(Length::Fill), update, dismiss]
        .spacing(8)
        .align_y(iced::Alignment::Center);
    container(bar)
        .padding([6, 10])
        .style(|_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(Color::from_rgb(0.90, 0.95, 1.0))),
            border: iced::Border {
                radius: 8.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.62, 0.78, 0.98),
            },
            ..Default::default()
        })
        .into()
}

/// Confirm-to-update popover: shows the version jump and downloads + installs +
/// relaunches on "Update now". Buttons disable while the install runs.
fn update_prompt_view<'a>(info: &UpdateInfo, installing: bool) -> Element<'a, Message> {
    let mut col = column![
        text("Update available").size(16),
        text(format!("v{} → {}", info.current, info.latest))
            .size(13)
            .color(TEXT_DARK),
        caption("Downloads the new version and relaunches sshoal."),
    ]
    .spacing(8);
    if installing {
        col = col.push(caption("Downloading and installing…"));
    }

    let mut update_now = button(text("Update now").size(13))
        .style(pill_button)
        .padding([5, 16]);
    if !installing {
        update_now = update_now.on_press(Message::InstallUpdate);
    }
    let actions = row![
        update_now,
        button(text("View").size(13))
            .style(pill_secondary)
            .padding([5, 16])
            .on_press(Message::OpenReleasePage),
        space().width(Length::Fill),
        button(text("Later").size(13))
            .style(pill_secondary)
            .padding([5, 16])
            .on_press(Message::CloseUpdatePrompt),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    col = col.push(actions);

    container(col)
        .padding(16)
        .width(Length::Fixed(320.0))
        .style(menu_box_style)
        .into()
}

/// The Preferences screen (reached from the tray's "Preferences…"): the
/// auto-update toggle, a manual "Check now", and an About line. Mirrors the
/// shape of [`ssh_list_view`].
fn prefs_view(app: &App) -> Element<'_, Message> {
    let header = row![
        tip(
            icon_button(ICON_CHEVRON_LEFT, 18.0, Message::ClosePrefs),
            "Back"
        ),
        screen_title("Preferences").width(Length::Fill),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let auto_row = row![
        text("Automatically check for updates")
            .size(14)
            .width(Length::Fill),
        toggler(app.settings.auto_update_enabled)
            .size(18)
            .on_toggle(Message::ToggleAutoUpdate),
    ]
    .align_y(iced::Alignment::Center);

    let check_label = if app.update_checking {
        "Checking…"
    } else {
        "Check now"
    };
    // Disabled (no on_press) while a check is in flight.
    let check = secondary_button(
        check_label,
        (!app.update_checking).then_some(Message::CheckUpdates),
    );

    let mut updates = column![
        auto_row,
        caption(
            "Checks GitHub for new releases on launch and notifies you. Updates are never installed automatically."
        ),
        check,
    ]
    .spacing(10);
    if let Some(status) = &app.update_status {
        updates = updates.push(caption(status.clone()));
    }
    if app.update_info.is_some() {
        updates = updates.push(primary_button("Update now", Message::PromptUpdate));
    }

    let mut backup = column![
        caption("Export tunnels to a portable file (encrypted by default), or import one."),
        row![
            primary_button("Export", Message::OpenExport),
            secondary_button("Import", Some(Message::OpenImport)),
        ]
        .spacing(8),
    ]
    .spacing(10);
    if let Some(status) = &app.backup_status {
        backup = backup.push(caption(status.clone()));
    }

    let about = column![
        text(format!("sshoal v{VERSION}")).size(13),
        row![
            secondary_button("View on GitHub", Some(Message::OpenReleasePage)),
            secondary_button("Report a bug", Some(Message::ReportBug)),
        ]
        .spacing(8),
    ]
    .spacing(10);

    // No right-only inset here (that's the tunnel list's trick to clear its
    // scrollbar): the cards should sit with equal left/right margins from the
    // window, governed solely by the symmetric outer padding below.
    let body = scrollable(
        column![
            pref_section("Updates", updates),
            pref_section("Backup", backup),
            pref_section("About", about),
        ]
        .spacing(18),
    )
    .direction(thin_scrollbar())
    .style(scroll_style)
    .height(Length::Fill);

    // Same white backdrop as the main screen — sections are set apart by the
    // white cards' border + soft shadow, not by a different screen colour.
    container(column![header, body].spacing(12))
        .padding(14)
        .into()
}

/// One Preferences section: an uppercase, semibold heading sitting above a light
/// "card" that groups the section's controls — so the heading reads clearly as a
/// heading, set apart from its contents.
fn pref_section<'a>(title: &str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let heading = section_title(title);
    // White card on the grey Preferences backdrop (set in `prefs_view`): the card
    // reads as a distinct group, and the secondary buttons inside — which are grey
    // — sit on white so they no longer blend into the section background.
    let card = container(content)
        .width(Length::Fill)
        .padding(12)
        .style(|_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(Color::WHITE)),
            border: iced::Border {
                radius: 10.0.into(),
                width: 1.0,
                color: Color::from_rgb(0.88, 0.88, 0.91),
            },
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, 0.05),
                offset: iced::Vector::new(0.0, 1.0),
                blur_radius: 3.0,
            },
            ..Default::default()
        });
    column![heading, card].spacing(6).into()
}

/// Open a URL in the user's default browser (shelling out, like the rest of the
/// app shells out to `ssh`/`osascript`). Best-effort — failures are logged only.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(not(target_os = "macos"))]
    let prog = "xdg-open";
    if let Err(e) = std::process::Command::new(prog).arg(url).spawn() {
        tracing::warn!(error = %e, url, "failed to open url");
    }
}

/// Relaunch sshoal after an in-place update. We detach a tiny shell that waits a
/// beat (so this process has fully exited and released LaunchServices) and then
/// reopens the freshly-installed app.
fn relaunch_app() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    #[cfg(target_os = "macos")]
    let cmd = match exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
    {
        Some(app) => format!("sleep 1; open '{}'", app.display()),
        None => format!("sleep 1; '{}' &", exe.display()),
    };
    #[cfg(not(target_os = "macos"))]
    let cmd = format!("sleep 1; '{}' &", exe.display());

    let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
}

/// A visible-but-subtle separator line between folder groups.
fn rule_style(_theme: &iced::Theme) -> iced::widget::rule::Style {
    iced::widget::rule::Style {
        color: Color::from_rgb(0.84, 0.84, 0.88),
        radius: 0.0.into(),
        fill_mode: iced::widget::rule::FillMode::Full,
        snap: true,
    }
}

/// Padding with only a right inset (the rest zero).
fn pad_r(right: f32) -> iced::Padding {
    iced::Padding {
        top: 0.0,
        right,
        bottom: 0.0,
        left: 0.0,
    }
}

/// A thin vertical scrollbar.
fn thin_scrollbar() -> iced::widget::scrollable::Direction {
    iced::widget::scrollable::Direction::Vertical(
        iced::widget::scrollable::Scrollbar::new()
            .width(6.0)
            .scroller_width(6.0)
            .margin(2.0),
    )
}

/// Rounded scrollbar rails + scroller.
fn scroll_style(
    theme: &iced::Theme,
    status: iced::widget::scrollable::Status,
) -> iced::widget::scrollable::Style {
    let mut style = iced::widget::scrollable::default(theme, status);
    for rail in [&mut style.vertical_rail, &mut style.horizontal_rail] {
        rail.border = iced::Border {
            radius: 4.0.into(),
            ..rail.border
        };
        rail.scroller.border = iced::Border {
            radius: 4.0.into(),
            ..rail.scroller.border
        };
    }
    style
}

fn tree_row<'a>(app: &App, d: &DisplayRow) -> Element<'a, Message> {
    let indent = space().width(Length::Fixed(d.depth as f32 * 10.0));

    // Folder: clicking the glyph 📁/📂 expands/collapses (as before); left-click
    // the name opens the folder dropdown.
    let Some(idx) = d.row_idx else {
        let expanded = !app.collapsed.contains(&d.path);
        let icon = if expanded {
            ICON_FOLDER_OPEN
        } else {
            ICON_FOLDER
        };
        // Folder is highlighted when it's in the selection or its dropdown is open.
        let selected = app.checked.contains(&d.path)
            || matches!(&app.context_menu, Some(ContextMenu::Folder(p)) if p == &d.path);
        let glyph = button(text(icon).font(LUCIDE).size(17.0).color(FOLDER_BLUE))
            .style(row_plain)
            .padding([2, 4])
            .on_press(Message::ClickFolder(d.path.clone()));
        let name_area = mouse_area(
            container(name_element(&d.name, 13.0, 28, TEXT_DARK))
                .width(Length::Fill)
                .padding([5, 4])
                .style(move |_t: &iced::Theme| iced::widget::container::Style {
                    background: selected
                        .then(|| iced::Background::Color(Color::from_rgb(0.80, 0.87, 1.0))),
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
        )
        .on_press(Message::FolderPress(d.path.clone()))
        .on_right_press(Message::FolderMenu(d.path.clone()));
        return row![indent, glyph, name_area]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .into();
    };

    // Leaf: the name area SELECTS on click (⌘/Shift to multi-select) and opens
    // the options dropdown on right-click — it never connects. The terminal icon
    // and the toggle (which does connect/disconnect) are separate controls.
    let checked = app.checked.contains(&d.path);
    let port = app.rows[idx].tunnel.local_port;
    // Fit the name to the actual column width so the ellipsis is always visible.
    // Reserve space for indent, dot, the :port label, the terminal + toggle, the
    // paddings/spacings and the scrollbar; the rest is ~6.6px per character.
    let port_w = format!(":{port}").len() as f32 * 6.0;
    let reserved =
        12.0 + 2.0 + 16.0 + (d.depth as f32 * 10.0) + 9.0 + 12.0 + port_w + 29.0 + 40.0 + 44.0;
    let name_max = ((app.window_width - reserved) / 7.3)
        .floor()
        .clamp(4.0, 60.0) as usize;
    let name_area = mouse_area(
        container(
            row![
                indent,
                status_dot(d.status),
                name_element(&d.name, 13.0, name_max, TEXT_DARK),
                space().width(Length::Fill),
                text(format!(":{port}"))
                    .size(11)
                    .color(Color::from_rgb(0.5, 0.5, 0.56)),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .padding([4, 6])
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: checked.then(|| iced::Background::Color(Color::from_rgb(0.80, 0.87, 1.0))),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        }),
    )
    .on_press(Message::RowPress(idx))
    .on_right_press(Message::RowRightPress(idx));

    let term = tip(
        icon_button(ICON_TERMINAL, 15.0, Message::OpenTerminal(idx)),
        "Open terminal",
    );
    let switch = tip(
        toggler(d.enabled)
            .size(17)
            .on_toggle(move |_| Message::ToggleTunnel(idx)),
        if d.enabled { "Disconnect" } else { "Connect" },
    );
    let line = row![name_area, term, switch]
        .spacing(8)
        .height(Length::Fixed(30.0))
        .align_y(iced::Alignment::Center);

    let mut col = column![line].spacing(2);
    if let Some((msg, _)) = &app.rows[idx].notice {
        col = col.push(
            row![
                space().width(Length::Fixed(d.depth as f32 * 10.0 + 24.0)),
                text(msg.clone())
                    .size(11)
                    .color(Color::from_rgb(0.80, 0.40, 0.16)),
            ]
            .spacing(4),
        );
    }
    col.into()
}

/// The floating options dropdown. For a tunnel selection: Edit (only when
/// exactly one is selected) + Delete (connect/disconnect lives on the row
/// toggle). For a folder: Connect all / Disconnect all / Delete.
fn menu_panel<'a>(app: &App, menu: &ContextMenu) -> Element<'a, Message> {
    let item = |label: &str, msg: Message, danger: bool| {
        let color = if danger {
            Color::from_rgb(0.85, 0.25, 0.25)
        } else {
            TEXT_DARK
        };
        button(text(label.to_string()).size(14).color(color))
            .style(row_plain)
            .width(Length::Fill)
            .padding([7, 12])
            .on_press(msg)
    };
    let mut items = column![].spacing(1);
    match menu {
        ContextMenu::Tunnel(_) => {
            // Count actual tunnels (the selection may include folder paths).
            let n = checked_indices(app).len();
            if n > 1 {
                // Multiple selected → bulk actions (no single toggle covers them).
                items = items.push(
                    text(format!("{n} selected"))
                        .size(11)
                        .color(Color::from_rgb(0.5, 0.5, 0.56)),
                );
                items = items.push(item("Connect all", Message::MenuConnect, false));
                items = items.push(item("Disconnect all", Message::MenuDisconnect, false));
            } else {
                items = items.push(item("Edit", Message::MenuEdit, false));
            }
            items = items.push(item("Delete", Message::MenuDelete, true));
        }
        ContextMenu::Folder(_) => {
            items = items.push(item("Connect all", Message::FolderConnectAll, false));
            items = items.push(item("Disconnect all", Message::FolderDisconnectAll, false));
            items = items.push(item("Rename", Message::FolderRename, false));
            items = items.push(item("Delete", Message::FolderDelete, true));
        }
    }

    container(items)
        .padding(4)
        .width(Length::Fixed(190.0))
        .style(menu_box_style)
        .into()
}

fn menu_box_style(_theme: &iced::Theme) -> iced::widget::container::Style {
    iced::widget::container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.99, 0.99, 1.0))),
        border: iced::Border {
            color: Color::from_rgb(0.80, 0.80, 0.84),
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: iced::Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.12),
            offset: iced::Vector::new(0.0, 2.0),
            blur_radius: 8.0,
        },
        ..Default::default()
    }
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

/// Leaf row, not selected: transparent, faint highlight on hover.
fn row_plain(_theme: &iced::Theme, status: button::Status) -> iced::widget::button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Color::from_rgb(0.95, 0.95, 0.97)),
        _ => None,
    };
    row_style(bg, 6.0)
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
fn name_element<'a>(name: &str, size: f32, max: usize, color: Color) -> Element<'a, Message> {
    let shown = truncate(name, max);
    // Shrink to the (already width-fitted) text, one line — so the ellipsis is
    // always fully rendered, never clipped.
    let label = text(shown.clone())
        .size(size)
        .color(color)
        .wrapping(text::Wrapping::None);
    if shown == name {
        label.into()
    } else {
        tip_text(label, name.to_string())
    }
}

fn tooltip_bubble(_theme: &iced::Theme) -> iced::widget::container::Style {
    // Light bubble (the default near-black one was hard to read).
    iced::widget::container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.99, 0.99, 1.0))),
        text_color: Some(TEXT_DARK),
        border: iced::Border {
            color: Color::from_rgb(0.78, 0.78, 0.82),
            width: 1.0,
            radius: 6.0.into(),
        },
        shadow: iced::Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.12),
            offset: iced::Vector::new(0.0, 2.0),
            blur_radius: 6.0,
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

// ---- reusable view components ----
//
// One definition per UI primitive (title, caption, label, button, input,
// dropdown) so every screen renders them identically and a style tweak lands in
// a single place. Prefer these over hand-rolling `text(...).size(...).color(...)`
// or `button(...).style(...)` inline.

/// A screen / dialog title — the large heading at the top of a screen.
fn screen_title<'a>(s: impl iced::widget::text::IntoFragment<'a>) -> Text<'a> {
    text(s).size(18)
}

/// A section heading: uppercase + semibold, set clearly apart from the body
/// below it (used by [`pref_section`]).
fn section_title<'a>(s: &str) -> Text<'a> {
    text(s.to_uppercase())
        .size(11)
        .font(Font {
            weight: iced::font::Weight::Bold,
            ..Font::default()
        })
        .color(TEXT_DARK)
}

/// Small, muted helper text: captions, sub-labels, status lines.
fn caption<'a>(s: impl iced::widget::text::IntoFragment<'a>) -> Text<'a> {
    text(s).size(11).color(TEXT_MUTED)
}

/// Inline error / validation text (red).
fn error_text<'a>(s: impl iced::widget::text::IntoFragment<'a>) -> Text<'a> {
    text(s).size(12).color(TEXT_DANGER)
}

/// Primary call-to-action button (filled).
fn primary_button<'a>(label: &'a str, on_press: Message) -> Element<'a, Message> {
    button(text(label).size(13))
        .style(pill_button)
        .padding([5, 16])
        .on_press(on_press)
        .into()
}

/// Secondary button (light, bordered). `on_press: None` renders it disabled
/// (e.g. "Checking…" while an update check is in flight).
fn secondary_button<'a>(label: &'a str, on_press: Option<Message>) -> Element<'a, Message> {
    let mut b = button(text(label).size(13))
        .style(pill_secondary)
        .padding([5, 16]);
    if let Some(msg) = on_press {
        b = b.on_press(msg);
    }
    b.into()
}

/// Destructive button (red) for deletes.
fn danger_button<'a>(label: &'a str, on_press: Message) -> Element<'a, Message> {
    button(text(label).size(13))
        .style(pill_danger)
        .padding([5, 16])
        .on_press(on_press)
        .into()
}

/// A labelled form row: a fixed-width label, then the control filling the rest.
fn labeled_field<'a>(
    label: &'a str,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        text(label).size(13).width(Length::Fixed(110.0)),
        control.into(),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center)
    .into()
}

/// A rounded single-line text input matching the app's form style.
fn text_field<'a>(
    placeholder: &'a str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    text_input(placeholder, value)
        .size(13)
        .padding([6, 9])
        .style(rounded_input)
        .on_input(on_input)
        .into()
}

/// A rounded dropdown (pick_list) matching the app's form style.
fn dropdown<'a>(
    options: Vec<String>,
    selected: Option<String>,
    placeholder: &'a str,
    on_select: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    pick_list(options, selected, on_select)
        .placeholder(placeholder)
        .padding([6, 9])
        .text_size(13)
        .style(rounded_pick)
        .width(Length::Fill)
        .into()
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

    let field = |label: &'a str, value: &'a str, f: Field, placeholder: &'a str| {
        labeled_field(
            label,
            text_field(placeholder, value, move |s| Message::EditField(f, s)),
        )
    };

    // SSH config: a dropdown of known configs (plus the current value if it
    // names an alias that isn't a saved config).
    let mut options = ssh_names.to_vec();
    if !form.ssh.is_empty() && !options.contains(&form.ssh) {
        options.push(form.ssh.clone());
    }
    let selected = (!form.ssh.is_empty()).then(|| form.ssh.clone());
    let ssh_field = labeled_field(
        "SSH config",
        dropdown(options, selected, "choose an SSH config", |name| {
            Message::EditField(Field::Ssh, name)
        }),
    );

    let mut col = column![
        screen_title(title),
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
        col = col.push(error_text(err.clone()));
    }

    // Verify the SSH target is reachable before saving. The button disables while
    // a probe runs or the form lacks an ssh target / remote host / remote port.
    let test_label = if form.testing {
        "Testing…"
    } else {
        "Test connection"
    };
    let test_btn = secondary_button(
        test_label,
        (!form.testing && test_ready(form)).then_some(Message::TestConnection),
    );
    let mut test_row = row![test_btn].spacing(8).align_y(iced::Alignment::Center);
    match &form.test_result {
        Some(Ok(())) => test_row = test_row.push(text("✓ Reachable").size(12).color(TEXT_SUCCESS)),
        Some(Err(reason)) => {
            test_row = test_row.push(text(format!("✗ {reason}")).size(12).color(TEXT_DANGER))
        }
        None => {}
    }
    col = col.push(test_row);

    let mut buttons = row![
        primary_button("Save", Message::SaveEdit),
        secondary_button("Cancel", Some(Message::CancelEdit)),
    ]
    .spacing(10);
    if let Some(idx) = form.target {
        buttons = buttons.push(space().width(Length::Fill));
        buttons = buttons.push(danger_button("Delete", Message::DeleteTunnel(idx)));
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
    persist_app(app);
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn ssh_list_view(app: &App) -> Element<'_, Message> {
    let header = row![
        tip(
            icon_button(ICON_CHEVRON_LEFT, 18.0, Message::CloseSshConfigs),
            "Back"
        ),
        screen_title("SSH configs").width(Length::Fill),
        tip(
            icon_button(ICON_PLUS, 18.0, Message::StartAddSsh),
            "Add SSH config"
        ),
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
        let content = column![text(c.name.clone()).size(14), caption(sub)]
            .spacing(2)
            .width(Length::Fill);
        // Click a row to edit; delete lives inside the edit form.
        list = list.push(
            button(content)
                .style(row_plain)
                .width(Length::Fill)
                .padding([6, 8])
                .on_press(Message::StartEditSsh(i)),
        );
    }

    let body = scrollable(container(list).padding(pad_r(16.0)))
        .direction(thin_scrollbar())
        .style(scroll_style)
        .height(Length::Fill);
    container(column![header, body].spacing(12))
        .padding(12)
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

fn ssh_edit_view<'a>(form: &'a SshForm) -> Element<'a, Message> {
    let title = if form.target.is_some() {
        "Edit SSH config"
    } else {
        "New SSH config"
    };

    let field = |label: &'a str, value: &'a str, f: SshField, placeholder: &'a str| {
        labeled_field(
            label,
            text_field(placeholder, value, move |s| Message::EditSshField(f, s)),
        )
    };

    let mut col = column![
        screen_title(title),
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
        col = col.push(error_text(err.clone()));
    }

    let mut buttons = row![
        primary_button("Save", Message::SaveSsh),
        secondary_button("Cancel", Some(Message::CancelSsh)),
    ]
    .spacing(10);
    if let Some(idx) = form.target {
        buttons = buttons.push(space().width(Length::Fill));
        buttons = buttons.push(danger_button("Delete", Message::DeleteSsh(idx)));
    }
    col = col.push(buttons);

    container(col).padding(16).into()
}

fn confirm_view(pending: &PendingDelete) -> Element<'_, Message> {
    let (title, listing): (String, Element<Message>) = match pending {
        PendingDelete::Tunnels(paths) => {
            let mut items = column![].spacing(2);
            for p in paths.iter().take(8) {
                items = items.push(text(format!("• {p}")).size(12).color(TEXT_DARK));
            }
            if paths.len() > 8 {
                items = items.push(caption(format!("…and {} more", paths.len() - 8)));
            }
            (format!("Delete {} tunnel(s)?", paths.len()), items.into())
        }
        PendingDelete::Ssh(name) => (
            "Delete SSH config?".to_string(),
            text(format!("• {name}")).size(12).color(TEXT_DARK).into(),
        ),
    };

    let col = column![
        screen_title(title),
        text("This can't be undone.").size(13),
        listing,
        row![
            danger_button("Delete", Message::ConfirmDelete),
            secondary_button("Cancel", Some(Message::CancelDelete)),
        ]
        .spacing(10),
    ]
    .spacing(10);
    // A floating popover (not a full screen).
    container(col)
        .padding(16)
        .width(Length::Fixed(300.0))
        .style(menu_box_style)
        .into()
}

/// The rename-folder popover: a single field (pre-filled with the folder's
/// current leaf name) plus Rename / Cancel. Enter in the field also confirms.
fn rename_view(form: &RenameFolder) -> Element<'_, Message> {
    let input = text_input("folder name", &form.name)
        .id(RENAME_ID)
        .size(13)
        .padding([6, 9])
        .style(rounded_input)
        .on_input(Message::RenameInput)
        .on_submit(Message::RenameConfirm);
    let mut col = column![
        screen_title("Rename folder"),
        caption(format!("Renaming “{}”", form.path)),
        input,
    ]
    .spacing(10);
    if let Some(err) = &form.error {
        col = col.push(error_text(err.clone()));
    }
    col = col.push(
        row![
            primary_button("Rename", Message::RenameConfirm),
            secondary_button("Cancel", Some(Message::RenameCancel)),
        ]
        .spacing(10),
    );
    container(col)
        .padding(16)
        .width(Length::Fixed(300.0))
        .style(menu_box_style)
        .into()
}

/// The export-options popover: encrypt + include-keys toggles, a passphrase field
/// (when encrypting), then "Save" which opens the native save dialog.
fn export_view(form: &ExportForm) -> Element<'_, Message> {
    let toggle_row = |label: &'static str, on: bool, msg: fn(bool) -> Message| {
        row![
            text(label).size(13).width(Length::Fill),
            toggler(on).size(18).on_toggle(msg),
        ]
        .align_y(iced::Alignment::Center)
    };
    let mut col = column![
        screen_title("Export tunnels"),
        caption("Save all tunnels (and the ssh configs they use) to a file."),
        toggle_row("Encrypt", form.encrypt, Message::ExportToggleEncrypt),
        toggle_row(
            "Include private keys",
            form.include_keys,
            Message::ExportToggleKeys
        ),
    ]
    .spacing(10);
    if form.encrypt {
        col = col.push(
            text_input("passphrase (min 12 chars)", &form.passphrase)
                .secure(true)
                .size(13)
                .padding([6, 9])
                .style(rounded_input)
                .on_input(Message::ExportPassphrase)
                .on_submit(Message::ExportPick),
        );
    } else if form.include_keys {
        col = col.push(error_text(
            "Unencrypted + private keys — anyone who gets this file gets your keys.",
        ));
    } else {
        col = col.push(caption(
            "Unencrypted — the file will contain hostnames and usernames.",
        ));
    }
    if let Some(err) = &form.error {
        col = col.push(error_text(err.clone()));
    }
    col = col.push(
        row![
            primary_button("Save", Message::ExportPick),
            secondary_button("Cancel", Some(Message::BackupCancel)),
        ]
        .spacing(10),
    );
    container(col)
        .padding(16)
        .width(Length::Fixed(320.0))
        .style(menu_box_style)
        .into()
}

/// The passphrase prompt for importing an encrypted file.
fn import_passphrase_view(form: &ImportForm) -> Element<'_, Message> {
    let mut col = column![
        screen_title("Import — passphrase"),
        caption("This file is encrypted. Enter its passphrase to import."),
        text_input("passphrase", &form.passphrase)
            .secure(true)
            .size(13)
            .padding([6, 9])
            .style(rounded_input)
            .on_input(Message::ImportPassphraseInput)
            .on_submit(Message::ImportConfirm),
    ]
    .spacing(10);
    if let Some(err) = &form.error {
        col = col.push(error_text(err.clone()));
    }
    col = col.push(
        row![
            primary_button("Import", Message::ImportConfirm),
            secondary_button("Cancel", Some(Message::BackupCancel)),
        ]
        .spacing(10),
    );
    container(col)
        .padding(16)
        .width(Length::Fixed(320.0))
        .style(menu_box_style)
        .into()
}

fn status_dot(state: TunnelState) -> Element<'static, Message> {
    // No dot when idle/disconnected — only show one once the tunnel is doing
    // something. Keep the slot width so names stay aligned either way.
    if state == TunnelState::Idle {
        return space().width(Length::Fixed(9.0)).into();
    }
    let color = match state {
        TunnelState::Up => Color::from_rgb(0.18, 0.80, 0.44),
        TunnelState::Connecting => Color::from_rgb(0.95, 0.77, 0.06),
        _ => Color::from_rgb(0.90, 0.42, 0.20), // reconnecting / failed
    };
    text("●").size(15).color(color).into()
}

fn subscription(_app: &App) -> Subscription<Message> {
    Subscription::batch([
        iced::time::every(Duration::from_millis(200)).map(|_| Message::Tick),
        iced::window::close_events().map(Message::WindowClosed),
        iced::event::listen_with(|event, _status, _window| {
            use iced::keyboard::{Event as Kbd, Key, key::Named};
            match event {
                iced::Event::Keyboard(Kbd::ModifiersChanged(m)) => {
                    Some(Message::ModifiersChanged(m))
                }
                iced::Event::Keyboard(Kbd::KeyPressed {
                    key: Key::Named(Named::ArrowUp),
                    ..
                }) => Some(Message::SelectDelta(-1)),
                iced::Event::Keyboard(Kbd::KeyPressed {
                    key: Key::Named(Named::ArrowDown),
                    ..
                }) => Some(Message::SelectDelta(1)),
                iced::Event::Keyboard(Kbd::KeyPressed {
                    key: Key::Named(Named::Enter),
                    ..
                }) => Some(Message::ActivateSelected),
                iced::Event::Keyboard(Kbd::KeyPressed {
                    key: Key::Named(Named::Escape),
                    ..
                }) => Some(Message::Escape),
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Message::CursorMoved(position))
                }
                iced::Event::Window(iced::window::Event::Resized(size)) => {
                    Some(Message::Resized(size.width))
                }
                _ => None,
            }
        }),
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
        size: Size::new(360.0, 620.0),
        min_size: Some(Size::new(300.0, 380.0)),
        ..window::Settings::default()
    }
}

fn build_tray() -> (TrayIcon, MenuIds) {
    let connect_all = MenuItem::new("Connect all", true, None);
    let open = MenuItem::new("Open sshoal", true, None);
    let prefs = MenuItem::new("Preferences…", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = MenuIds {
        connect_all: connect_all.id().clone(),
        open: open.id().clone(),
        prefs: prefs.id().clone(),
        quit: quit.id().clone(),
    };
    let tray_menu =
        Menu::with_items(&[&connect_all, &open, &prefs, &quit]).expect("build tray menu");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_menu_on_left_click(false)
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
