//! Register sshoal to launch when the user logs in, so its tunnels reconnect
//! after a reboot without anyone reopening the app.
//!
//! Both platforms use a plain *login-item file* the OS reads at login — no
//! permission prompt, no extra dependency, and it works whether sshoal runs as a
//! packaged `.app` or a bare binary:
//!   * **macOS** — a LaunchAgent plist at
//!     `~/Library/LaunchAgents/dev.japananh.sshoal.plist`.
//!   * **Linux** — an XDG autostart entry at `~/.config/autostart/sshoal.desktop`.
//!
//! Enabling writes the file (pointing at the current executable); disabling
//! removes it; [`is_enabled`] is "does the file exist". The path/render/remove
//! logic lives in pure `*_in(home, exe)` helpers so it's unit-tested against a
//! temp directory with no real OS calls.

use std::path::{Path, PathBuf};

/// macOS LaunchAgent label — matches the `.app` bundle id set in
/// `scripts/package-macos.sh`.
#[cfg(target_os = "macos")]
const MACOS_LABEL: &str = "dev.japananh.sshoal";

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    #[error("HOME is not set, so the login-item path can't be resolved")]
    NoHome,
    #[error("resolving the running executable: {0}")]
    Exe(#[source] std::io::Error),
    #[error("open at login isn't supported on this platform")]
    Unsupported,
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// The login-item file for this platform under `home`, or `None` on a platform
/// sshoal doesn't support autostart for.
fn login_item_path_in(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    return Some(
        home.join("Library/LaunchAgents")
            .join(format!("{MACOS_LABEL}.plist")),
    );
    #[cfg(target_os = "linux")]
    return Some(home.join(".config/autostart/sshoal.desktop"));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = home;
        None
    }
}

/// The file contents that register `exe` to launch at login.
fn render_entry(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    #[cfg(target_os = "macos")]
    return format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{MACOS_LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{exe}</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         </dict>\n\
         </plist>\n",
        exe = xml_escape(&exe),
    );
    #[cfg(target_os = "linux")]
    return format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=sshoal\n\
         Exec={exe}\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden=false\n"
    );
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = exe;
        String::new()
    }
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn is_enabled_in(home: &Path) -> bool {
    login_item_path_in(home).is_some_and(|p| p.exists())
}

fn set_enabled_in(home: &Path, exe: &Path, on: bool) -> Result<(), AutostartError> {
    let path = login_item_path_in(home).ok_or(AutostartError::Unsupported)?;
    if on {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| AutostartError::Io {
                action: "creating",
                path: dir.display().to_string(),
                source,
            })?;
        }
        std::fs::write(&path, render_entry(exe)).map_err(|source| AutostartError::Io {
            action: "writing",
            path: path.display().to_string(),
            source,
        })?;
    } else if let Err(source) = std::fs::remove_file(&path) {
        // Already-off is success, not an error.
        if source.kind() != std::io::ErrorKind::NotFound {
            return Err(AutostartError::Io {
                action: "removing",
                path: path.display().to_string(),
                source,
            });
        }
    }
    Ok(())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| !h.as_os_str().is_empty())
}

/// Whether sshoal is currently registered to open at login.
pub fn is_enabled() -> bool {
    home().is_some_and(|h| is_enabled_in(&h))
}

/// Register (`on = true`) or unregister (`on = false`) sshoal from launching at
/// login. Idempotent: enabling twice rewrites the file, disabling when already
/// off is a no-op.
pub fn set_enabled(on: bool) -> Result<(), AutostartError> {
    let home = home().ok_or(AutostartError::NoHome)?;
    let exe = std::env::current_exe().map_err(AutostartError::Exe)?;
    set_enabled_in(&home, &exe, on)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sshoal-autostart-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enable_writes_then_disable_removes_the_login_item() {
        let home = temp_home("toggle");
        let exe = Path::new("/opt/sshoal/bin/sshoal");
        let path = login_item_path_in(&home).expect("supported platform");

        assert!(!is_enabled_in(&home), "starts disabled");

        set_enabled_in(&home, exe, true).unwrap();
        assert!(is_enabled_in(&home), "enabled after write");
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("/opt/sshoal/bin/sshoal"), "points at the exe");

        set_enabled_in(&home, exe, false).unwrap();
        assert!(!is_enabled_in(&home), "disabled after remove");
        assert!(!path.exists());

        // Disabling when already off is a no-op, not an error.
        set_enabled_in(&home, exe, false).unwrap();

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn enable_creates_missing_parent_dirs() {
        let home = temp_home("mkdir");
        // Neither Library/LaunchAgents nor .config/autostart exists yet.
        set_enabled_in(&home, Path::new("/usr/local/bin/sshoal"), true).unwrap();
        assert!(is_enabled_in(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn read_only_public_helpers_are_safe_to_call() {
        // `home()` reads $HOME and `is_enabled()` only checks whether the
        // login-item file exists — both read-only, no side effects. (We don't
        // call `set_enabled`, which would write to the real ~/Library.)
        let _ = home();
        let _ = is_enabled();
    }
}
