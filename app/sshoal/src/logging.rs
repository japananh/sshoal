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

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// Installs the global subscriber. Honors `RUST_LOG`; by default shows our own
/// crates down to `debug` and silences chatty dependencies below `warn`.
pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,sshoal=debug,sshoal_core=debug"));

    tracing_subscriber::fmt()
        .event_format(Logrus)
        .with_env_filter(filter)
        .init();
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
