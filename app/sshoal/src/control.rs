//! CLI ⇄ daemon control channel over a Unix socket, so `sshoal connect PATH`
//! (and friends) can drive the running tray daemon without opening the app —
//! e.g. a script or an AI agent turning a tunnel on before hitting a database.
//!
//! The daemon owns the tunnel supervisors, so a separate short-lived CLI process
//! can't flip one directly. It sends a one-line command over the socket at
//! `~/.config/sshoal/control.sock`; the daemon applies it and replies. The
//! daemon side is split in two because iced's `update` loop is the only place
//! that may touch `App` and it must never block: the async **listener** (here)
//! parks each request on a shared queue and awaits a reply, and the update loop
//! drains that queue on its periodic tick, runs the command, and sends the reply
//! back. The CLI side is plain blocking I/O — no async runtime needed.
//!
//! ## Wire format (one request per connection)
//! Request: a single line `<verb> [PATH]`.
//! Response: first line `OK` or `ERR\t<message>`; then, for `OK`, one line per
//! tunnel `<state>\t<path>\t<local_port>\t<note>` (note may be empty). The
//! daemon closes the connection when done.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::config_path;

/// `~/.config/sshoal/control.sock`, beside the config.
pub fn socket_path() -> PathBuf {
    config_path().with_file_name("control.sock")
}

/// A command parked by the listener for the update loop to apply. The reply
/// channel lives here (not inside an iced `Message`) so the sender never has to
/// be `Clone`.
pub struct ControlRequest {
    pub cmd: String,
    pub reply: tokio::sync::oneshot::Sender<String>,
}

/// Shared hand-off between the async listener and the (sync) update-loop drain.
/// A `std::sync::Mutex` is fine: the listener never holds it across an `.await`.
pub type Queue = std::sync::Arc<std::sync::Mutex<Vec<ControlRequest>>>;

// ───────────────────────── daemon side ─────────────────────────

/// Bind the control socket and serve requests onto `queue`, on `runtime`.
pub fn spawn_listener(runtime: &tokio::runtime::Runtime, queue: Queue) {
    let path = socket_path();
    runtime.spawn(async move { listen(path, queue).await });
}

async fn listen(path: PathBuf, queue: Queue) {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    // The single-instance lock guarantees we're the only daemon, so any socket
    // file already here is stale — remove it before binding (sync; one-shot).
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "control socket bind failed; CLI control disabled");
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    tracing::info!(path = %path.display(), "control socket listening");

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let queue = queue.clone();
        // One task per connection so a slow client can't stall the others.
        tokio::spawn(async move { serve(stream, queue).await });
    }
}

async fn serve(mut stream: tokio::net::UnixStream, queue: Queue) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read the one-line command (stop at the newline; cap it so a rogue client
    // can't make us buffer forever).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() > 4096 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let cmd = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let Ok(mut q) = queue.lock() else { return };
        q.push(ControlRequest { cmd, reply: tx });
    } // drop the guard before awaiting — never hold a std Mutex across .await
    let resp = match tokio::time::timeout(Duration::from_secs(5), rx).await {
        Ok(Ok(s)) => s,
        _ => "ERR\tdaemon did not respond\n".to_string(),
    };
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ───────────────────────── CLI side ─────────────────────────

/// Send one request and read the whole reply. `Err` means the daemon isn't
/// reachable (not running, or no socket yet).
fn send(request: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(request.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut resp = String::new();
    stream.read_to_string(&mut resp)?;
    Ok(resp)
}

fn not_running() -> i32 {
    eprintln!(
        "sshoal: the app isn't running (nothing to control).\n\
         Open it (Spotlight → sshoal, or `open -a sshoal`), or enable \
         Preferences → Open at login."
    );
    3
}

/// A parsed `<state>\t<path>\t<port>\t<note>` reply line.
#[derive(Debug)]
struct Row {
    state: String,
    path: String,
    port: String,
    note: String,
}

/// Split an `OK`/`ERR` reply into its rows. `Err(msg)` for an `ERR` reply or a
/// malformed one.
fn parse(resp: &str) -> Result<Vec<Row>, String> {
    let mut lines = resp.lines();
    match lines.next() {
        Some("OK") => {}
        Some(l) => {
            let msg = l
                .strip_prefix("ERR\t")
                .unwrap_or(l.trim_start_matches("ERR"));
            return Err(if msg.is_empty() {
                "unexpected response from daemon".to_string()
            } else {
                msg.to_string()
            });
        }
        None => return Err("empty response from daemon".to_string()),
    }
    Ok(lines
        .map(|l| {
            let mut f = l.splitn(4, '\t');
            Row {
                state: f.next().unwrap_or("").to_string(),
                path: f.next().unwrap_or("").to_string(),
                port: f.next().unwrap_or("").to_string(),
                note: f.next().unwrap_or("").to_string(),
            }
        })
        .collect())
}

fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("(no matching tunnels)");
        return;
    }
    let wp = rows.iter().map(|r| r.path.len()).max().unwrap_or(4).max(4);
    for r in rows {
        let port = if r.port == "0" {
            String::new()
        } else {
            format!(":{}", r.port)
        };
        let note = if r.note.is_empty() {
            String::new()
        } else {
            format!("  {}", r.note)
        };
        println!(
            "{:<12} {:<wp$} {:>6}{}",
            r.state,
            r.path,
            port,
            note,
            wp = wp
        );
    }
}

/// `sshoal list` — every tunnel and its state.
pub fn run_list() -> i32 {
    match send("list") {
        Err(_) => not_running(),
        Ok(resp) => match parse(&resp) {
            Ok(rows) => {
                print_table(&rows);
                0
            }
            Err(msg) => {
                eprintln!("sshoal: {msg}");
                1
            }
        },
    }
}

/// `sshoal status PATH` — state of the tunnel(s) under PATH.
pub fn run_status(args: &[String]) -> i32 {
    let Some(path) = args.first() else {
        eprintln!("sshoal status: needs a tunnel path or folder");
        return 2;
    };
    match send(&format!("status {path}")) {
        Err(_) => not_running(),
        Ok(resp) => match parse(&resp) {
            Ok(rows) => {
                print_table(&rows);
                0
            }
            Err(msg) => {
                eprintln!("sshoal: {msg}");
                1
            }
        },
    }
}

/// The `--flag VALUE` pairs `sshoal set` accepts, mapped to wire field names.
const SET_FLAGS: [(&str, &str); 6] = [
    ("--folder", "folder"),
    ("--name", "name"),
    ("--ssh", "ssh"),
    ("--local-port", "local-port"),
    ("--remote-host", "remote-host"),
    ("--remote-port", "remote-port"),
];

/// Turn `--local-port 6000 …` into wire `key=value` fields. `Err` is a message
/// for the user (unknown flag, missing value, or an unusable character).
fn parse_set_args(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut path: Option<String> = None;
    let mut pairs = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some((_, key)) = SET_FLAGS.iter().find(|(flag, _)| flag == arg) {
            let Some(value) = it.next() else {
                return Err(format!("{arg} needs a value"));
            };
            // Tabs/newlines are the wire framing — refuse rather than corrupt it.
            if value.contains(['\t', '\n']) {
                return Err(format!("{arg} value can't contain tabs or newlines"));
            }
            pairs.push(format!("{key}={value}"));
        } else if arg.starts_with('-') {
            return Err(format!("unknown option \"{arg}\""));
        } else if path.is_none() {
            path = Some(arg.clone());
        } else {
            return Err(format!("unexpected argument \"{arg}\""));
        }
    }
    Ok((path, pairs))
}

/// `sshoal set PATH --local-port N …` — edit a tunnel's fields (the same ones
/// the in-app edit screen has). A connected tunnel is reconnected on the new
/// settings.
pub fn run_set(args: &[String]) -> i32 {
    let (path, pairs) = match parse_set_args(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("sshoal set: {msg}");
            return 2;
        }
    };
    let Some(path) = path else {
        eprintln!(
            "sshoal set: needs a tunnel path\n  \
             e.g. sshoal set gc/dev/db/app-api --local-port 54399"
        );
        return 2;
    };
    if pairs.is_empty() {
        eprintln!(
            "sshoal set: needs at least one field to change\n  \
             --folder --name --ssh --local-port --remote-host --remote-port"
        );
        return 2;
    }

    let request = format!("set\t{path}\t{}", pairs.join("\t"));
    match send(&request) {
        Err(_) => not_running(),
        Ok(resp) => match parse(&resp) {
            Ok(rows) => {
                print_table(&rows);
                0
            }
            Err(msg) => {
                eprintln!("sshoal: {msg}");
                1
            }
        },
    }
}

/// `sshoal disconnect PATH` — tear down the tunnel(s) under PATH.
pub fn run_disconnect(args: &[String]) -> i32 {
    let Some(path) = args.iter().find(|a| !a.starts_with('-')) else {
        eprintln!("sshoal disconnect: needs a tunnel path or folder");
        return 2;
    };
    match send(&format!("disconnect {path}")) {
        Err(_) => not_running(),
        Ok(resp) => match parse(&resp) {
            Ok(rows) => {
                println!("disconnected {} tunnel(s)", rows.len());
                0
            }
            Err(msg) => {
                eprintln!("sshoal: {msg}");
                1
            }
        },
    }
}

/// `sshoal connect PATH [--no-wait] [--timeout SECS]` — bring up the tunnel(s)
/// under PATH. By default it waits until each is up (or errors / times out),
/// polling the daemon, and exits non-zero if any didn't come up — so a caller
/// knows whether the port is actually usable.
pub fn run_connect(args: &[String]) -> i32 {
    let mut path: Option<&str> = None;
    let mut wait = true;
    let mut timeout = Duration::from_secs(15);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--no-wait" => wait = false,
            "--timeout" => {
                timeout = it
                    .next()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(timeout);
            }
            other if !other.starts_with('-') => path = Some(other),
            _ => {}
        }
    }
    let Some(path) = path else {
        eprintln!("sshoal connect: needs a tunnel path or folder");
        return 2;
    };

    let resp = match send(&format!("connect {path}")) {
        Err(_) => return not_running(),
        Ok(r) => r,
    };
    let targets = match parse(&resp) {
        Ok(rows) => rows,
        Err(msg) => {
            eprintln!("sshoal: {msg}");
            return 1;
        }
    };
    if !wait {
        print_table(&targets);
        return 0;
    }

    // Poll until every matched tunnel is settled: `up` (good) or `off` (no
    // supervisor — it will never come up, e.g. a port conflict). `off` is a
    // durable signal, unlike the note, which the daemon clears after a few
    // seconds. Everything else (connecting/reconnecting/failed) may still recover.
    let deadline = Instant::now() + timeout;
    let mut rows = targets;
    loop {
        let settled = rows.iter().all(|r| r.state == "up" || r.state == "off");
        if settled || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        match send(&format!("status {path}")).ok().as_deref().map(parse) {
            Some(Ok(fresh)) => rows = fresh,
            _ => break,
        }
    }

    print_table(&rows);
    let all_up = rows.iter().all(|r| r.state == "up");
    if all_up { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_ok_rows_and_err() {
        let rows = parse("OK\nup\tgc/dev/db\t6001\t\nfailed\tgc/prod/db\t7001\tport busy\n")
            .expect("OK parses");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].state, "up");
        assert_eq!(rows[0].path, "gc/dev/db");
        assert_eq!(rows[0].port, "6001");
        assert!(rows[0].note.is_empty());
        assert_eq!(rows[1].note, "port busy");

        // An ERR reply surfaces its message; an empty body is an error too.
        assert_eq!(
            parse("ERR\tno tunnel matches \"x\"\n")
                .unwrap_err()
                .as_str(),
            "no tunnel matches \"x\""
        );
        assert!(parse("").is_err());
    }

    #[test]
    fn set_args_become_wire_pairs() {
        let args: Vec<String> = ["gc/dev/db", "--local-port", "6001", "--remote-host", "a b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (path, pairs) = parse_set_args(&args).expect("parses");
        assert_eq!(path.as_deref(), Some("gc/dev/db"));
        // Values keep spaces; order follows the command line.
        assert_eq!(pairs, vec!["local-port=6001", "remote-host=a b"]);

        let err = |a: &[&str]| {
            parse_set_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap_err()
        };
        assert!(err(&["p", "--nope", "1"]).contains("unknown option"));
        assert!(err(&["p", "--local-port"]).contains("needs a value"));
        assert!(err(&["p", "--name", "a\tb"]).contains("tabs"));
        assert!(err(&["p", "q"]).contains("unexpected argument"));

        // A bare path with no flags parses, but yields nothing to change.
        let (p, pairs) = parse_set_args(&["p".to_string()]).unwrap();
        assert_eq!(p.as_deref(), Some("p"));
        assert!(pairs.is_empty());
    }
}
