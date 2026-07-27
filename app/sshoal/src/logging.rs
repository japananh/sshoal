//! logrus-style (TTY) log formatting for `tracing`.
//!
//! Go's logrus prints one line per event in its terminal format:
//!
//! ```text
//! INFO[2026-06-07T15:04:05+07:00] alive tick=16 window=None
//! ```
//!
//! `tracing-subscriber` has no built-in formatter that matches this, so we
//! implement `FormatEvent` ourselves: a 4-char upper-case level (like logrus:
//! INFO / WARN / ERRO / DEBU / TRAC), a bracketed local RFC3339 timestamp, the
//! raw message, then every structured field as `key=value` with logrus-style
//! quoting.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// User-visible log location: rolling `sshoal.log` plus append-only `crash.log`.
/// macOS keeps user logs under ~/Library/Logs; elsewhere sit beside the config.
pub fn log_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    return home.join("Library/Logs/sshoal");
    #[cfg(not(target_os = "macos"))]
    return home.join(".config/sshoal/logs");
}

/// Installs the global subscriber, a crash-logging panic hook, and — when there
/// is no terminal (i.e. launched as an app) — redirects stdout/stderr to a file.
/// Honors `RUST_LOG`; by default shows our own crates down to `debug` and
/// silences chatty dependencies below `warn`.
pub fn init() {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    install_panic_hook(dir.join("crash.log"));

    // A tray daemon has no console, so its stdout/stderr — tracing output *and*
    // the panic message on an unwind — would vanish, leaving a crash untraceable
    // (`panic = "unwind"` writes no macOS crash report). Redirect both to a file
    // so the next quit is diagnosable. Skip it under a terminal (a dev run) so
    // logs still stream to the console.
    #[cfg(unix)]
    if unsafe { libc::isatty(1) } == 0 {
        redirect_stdio(&dir);
    }

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,sshoal=debug,sshoal_core=debug"));

    tracing_subscriber::fmt()
        .event_format(Logrus)
        .with_env_filter(filter)
        .init();
}

/// Append a structured record to `crash.log` on every panic — the daemon's only
/// durable evidence of an unwind. Chains the previous hook so the default
/// message still reaches stderr (→ the redirected log). The panic *location*
/// (file:line) is embedded regardless of `strip`, so it pins the site even with
/// release symbols stripped.
fn install_panic_hook(crash_log: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z");
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = info
            .payload_as_str()
            .unwrap_or("<non-string panic payload>");
        let backtrace = std::backtrace::Backtrace::force_capture();
        let record = format!(
            "\n===== PANIC {now} =====\n\
             thread:    {thread}\n\
             location:  {location}\n\
             message:   {message}\n\
             backtrace:\n{backtrace}\n"
        );
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&crash_log)
        {
            let _ = f.write_all(record.as_bytes());
        }
        previous(info);
    }));
}

/// Point fd 1 and 2 at `dir/sshoal.log` (append), rolling one generation past
/// ~5 MB so it can't grow without bound. Rust's `Stdout` is line-buffered and
/// `Stderr` unbuffered, so each log line and the panic message hit disk before
/// an unwind exits — nothing is lost.
#[cfg(unix)]
fn redirect_stdio(dir: &std::path::Path) {
    use std::os::unix::io::AsRawFd;
    let path = dir.join("sshoal.log");
    if fs::metadata(&path)
        .map(|m| m.len() > 5 * 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, dir.join("sshoal.log.1"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    // dup2 gives fds 1/2 their own descriptor onto this file; dropping `file`
    // then closes only the original, leaving stdout/stderr valid.
    let fd = file.as_raw_fd();
    unsafe {
        libc::dup2(fd, 1);
        libc::dup2(fd, 2);
    }
}

struct Logrus;

impl<S, N> FormatEvent<S, N> for Logrus
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let time = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z");
        // logrus truncates the upper-case level to 4 chars: INFO/WARN/ERRO/DEBU/TRAC.
        let level = event.metadata().level().as_str();
        let level = &level[..level.len().min(4)];

        let mut visitor = LogrusVisitor::default();
        event.record(&mut visitor);

        write!(writer, "{level}[{time}]")?;
        // The message is positional in logrus' TTY format — printed raw, unquoted.
        if let Some(msg) = &visitor.message {
            write!(writer, " {msg}")?;
        }
        for (key, value) in &visitor.fields {
            write!(writer, " {key}={}", quote(value))?;
        }
        writeln!(writer)
    }
}

/// Pulls the `message` field aside and collects the rest as ordered pairs.
#[derive(Default)]
struct LogrusVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl LogrusVisitor {
    fn put(&mut self, field: &Field, value: String) {
        let name = field.name();
        if name == "message" {
            self.message = Some(value);
        } else if name.starts_with("log.") {
            // Metadata injected by the `log` -> `tracing` bridge for non-tracing
            // dependencies (log.target/module_path/file/line) — pure noise.
        } else {
            self.fields.push((name.to_string(), value));
        }
    }
}

impl Visit for LogrusVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }
}

/// logrus quotes a value unless every character is "safe" — we mirror that so
/// `tick=16` stays bare while `msg="two words"` gets quoted and escaped.
fn quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/' | '@' | '^' | '+')
        });

    if safe {
        s.to_string()
    } else {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The panic hook writing crash.log is the whole point of this module — a
    // silent unwind leaves no other trace — so prove it actually records the
    // message and the source location. It installs a process-global hook; other
    // tests don't panic, so the append-only file only gets our synthetic record.
    #[test]
    fn panic_hook_writes_a_crash_record() {
        let dir = std::env::temp_dir().join(format!("sshoal-crash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("crash.log");
        let _ = std::fs::remove_file(&log);

        install_panic_hook(log.clone());
        // Swallow the unwind so the test itself survives; the hook still fires.
        let _ = std::panic::catch_unwind(|| panic!("synthetic-crash-marker"));

        let text = std::fs::read_to_string(&log).expect("crash.log written");
        assert!(text.contains("synthetic-crash-marker"), "message captured");
        assert!(text.contains("logging.rs"), "location captured");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
