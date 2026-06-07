//! sshoal — risk-first spike.
//!
//! Goal of this file: prove the single riskiest seam before building anything
//! else — can `iced::daemon` (a tray-resident app with no window at launch)
//! coexist with a `tray-icon` menu, keep doing background work while the window
//! is closed, and quit cleanly from the tray? Everything else in sshoal is
//! ordinary, testable Rust; this is the part nothing guarantees will compose.

mod logging;

use std::time::Duration;

use iced::widget::{column, text};
use iced::{window, Element, Subscription, Task};
use tracing::info;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone)]
enum Message {
    /// Periodic heartbeat — drives "is the daemon still alive with no window?"
    /// and doubles as our poll of the tray menu event channel.
    Tick,
    WindowOpened(window::Id),
}

struct App {
    /// Kept alive for the whole process — dropping it removes the tray icon.
    _tray: TrayIcon,
    open_id: MenuId,
    quit_id: MenuId,
    window: Option<window::Id>,
    ticks: u64,
    /// When `SSHOAL_SELFTEST=1`, the app drives the open → close → quit
    /// lifecycle itself so it can be verified headlessly (no human clicks).
    selftest: bool,
}

/// A flat 32×32 teal square — good enough to see something in the menu bar.
fn make_icon() -> Icon {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        rgba.extend_from_slice(&[0x2e, 0xc4, 0xb6, 0xff]);
    }
    Icon::from_rgba(rgba, w, h).expect("valid rgba icon")
}

fn boot() -> (App, Task<Message>) {
    let open = MenuItem::new("Open sshoal", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let open_id = open.id().clone();
    let quit_id = quit.id().clone();

    let menu = Menu::with_items(&[&open, &quit]).expect("build tray menu");
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(make_icon())
        .with_tooltip("sshoal")
        .build()
        .expect("build tray icon");

    // Task::none() => no window at launch. The daemon runs silently in the
    // menu bar until the user picks "Open sshoal".
    (
        App {
            _tray: tray,
            open_id,
            quit_id,
            window: None,
            ticks: 0,
            selftest: std::env::var("SSHOAL_SELFTEST").as_deref() == Ok("1"),
        },
        Task::none(),
    )
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            app.ticks += 1;
            if app.ticks % 16 == 0 {
                info!(tick = app.ticks, window = ?app.window, "alive");
            }

            // Headless lifecycle drive: open, then close (daemon must survive),
            // then quit — each verifiable from the logs without a human.
            if app.selftest {
                match app.ticks {
                    16 if app.window.is_none() => {
                        info!("selftest: opening window");
                        let (id, task) = window::open(window::Settings::default());
                        app.window = Some(id);
                        return task.map(Message::WindowOpened);
                    }
                    40 => {
                        if let Some(id) = app.window.take() {
                            info!("selftest: closing window (daemon should survive)");
                            return window::close(id);
                        }
                    }
                    64 => {
                        info!("selftest: quitting");
                        return iced::exit();
                    }
                    _ => {}
                }
            }

            // Drain tray menu clicks (global crossbeam channel from tray-icon).
            let rx = MenuEvent::receiver();
            while let Ok(event) = rx.try_recv() {
                if event.id == app.open_id {
                    if app.window.is_none() {
                        let (id, task) = window::open(window::Settings::default());
                        app.window = Some(id);
                        return task.map(Message::WindowOpened);
                    }
                } else if event.id == app.quit_id {
                    return iced::exit();
                }
            }
            Task::none()
        }
        Message::WindowOpened(id) => {
            info!(window = ?id, "window opened");
            Task::none()
        }
    }
}

fn view(app: &App, _window: window::Id) -> Element<'_, Message> {
    column![
        text("sshoal — spike").size(24),
        text(format!("background ticks: {}", app.ticks)),
        text("Close this window: the tray icon stays and ticks keep running."),
        text("Quit only from the tray menu."),
    ]
    .spacing(12)
    .padding(20)
    .into()
}

fn subscription(_app: &App) -> Subscription<Message> {
    iced::time::every(Duration::from_millis(120)).map(|_| Message::Tick)
}

fn main() -> iced::Result {
    logging::init();
    iced::daemon(boot, update, view)
        .subscription(subscription)
        .title(|_app: &App, _id| String::from("sshoal"))
        .run()
}
