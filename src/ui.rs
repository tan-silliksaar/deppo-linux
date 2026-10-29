use std::collections::VecDeque;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cache;
use crate::config::{self, Subscription};
use crate::host;
use crate::upload;

const INDEX: &str = include_str!("../ui/index.html");
const STYLE: &str = include_str!("../ui/style.css");
const SCRIPT: &str = include_str!("../ui/script.js");
const FAVICON: &str = include_str!("../ui/favicon.svg");
const IGNORE_TEMPLATE: &str = include_str!("default.deppoignore");
const BODY_LIMIT: usize = 1_000_000;
const PING_EVERY: Duration = Duration::from_secs(15);

struct App {
    running: AtomicBool,
    live: Mutex<Live>,
}

struct Live {
    run: u64,
    next: u64,
    lines: VecDeque<(u64, u64, String)>,
    done: Option<(u64, u64, bool)>,
}

struct Incoming {
    method: String,
    target: String,
    body: Vec<u8>,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Run {
    counts: Counts,
    summary: String,
    ran_at: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Counts {
    uploaded: u64,
    already: u64,
    unchanged: u64,
    failed: u64,
}

/// Listen on 127.0.0.1 and keep the page up for the life of this process.
pub fn start() -> Result<String> {
    let app = Arc::new(App {
        running: AtomicBool::new(false),
        live: Mutex::new(Live { run: 0, next: 0, lines: VecDeque::new(), done: None }),
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("listen on 127.0.0.1")?;
    let port = listener.local_addr().context("local address")?.port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("deppo: {err}");
                    continue;
                }
            };
            let app = Arc::clone(&app);
            thread::spawn(move || {
                if let Err(err) = handle(stream, &app) {
                    let text = format!("{err:#}");
                    if !text.contains("timed out") && !text.contains("connection") && !text.contains("broken pipe") && !text.contains("reset") {
                        eprintln!("deppo: {text}");
                    }
                }
            });
        }
    });
    Ok(format!("http://127.0.0.1:{port}/"))
}

pub fn open_browser(url: &str) {
    for program in ["xdg-open", "gio", "gnome-open"] {
        let mut command = Command::new(program);
        command.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if command.spawn().is_ok() {
            return;
        }
    }
    eprintln!("deppo: could not open the browser. Open {url}");
}

fn handle(stream: TcpStream, app: &Arc<App>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut reader = BufReader::new(stream);
    let incoming = read_incoming(&mut reader)?;
    let mut stream = reader.into_inner();
    stream.set_read_timeout(None)?;
    let (path, query) = split_target(&incoming.target);
    if incoming.method == "GET" && path == "/events" {
        return serve_events(stream, app);
    }
    let (status, content_type, body) = respond(app, &incoming.method, path, query, &incoming.body);
    write_bytes(&mut stream, status, content_type, &body)
}

fn respond(app: &Arc<App>, method: &str, path: &str, query: &str, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
    match (method, path) {
        ("GET", "/" | "/index.html") => (200, "text/html; charset=utf-8", INDEX.as_bytes().to_vec()),
        ("GET", "/style.css") => (200, "text/css; charset=utf-8", STYLE.as_bytes().to_vec()),
        ("GET", "/script.js") => (200, "text/javascript; charset=utf-8", SCRIPT.as_bytes().to_vec()),
        ("GET", "/favicon.svg" | "/favicon.ico") => (200, "image/svg+xml", FAVICON.as_bytes().to_vec()),
        ("GET", "/api/info") => json_status(200, info(app)),
        ("POST", "/api/subscriptions") => match add_subscription(body) {
            Ok(value) => json_status(200, value),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("DELETE", "/api/subscriptions") => match delete_subscription(body) {
            Ok(()) => json_status(200, json!({ "ok": true })),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("POST", "/api/mount") => match assign_mount(body) {
            Ok(value) => json_status(200, value),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("POST", "/api/startup") => match enable_startup() {
            Ok(()) => json_status(200, json!({ "ok": true })),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("POST", "/api/timer") => match save_timer(body) {
            Ok(value) => json_status(200, value),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("POST", "/api/backup") => match start_backup(app, body) {
            Ok(()) => json_status(200, json!({ "ok": true })),
            Err(err) => json_status(if err == "A backup is already running." { 409 } else { 400 }, json!({ "error": err })),
        },
        ("POST", "/api/folders") => match folder_target(body).and_then(|(sub, path)| add_folder(&sub, &path)) {
            Ok(path) => json_status(200, json!({ "path": path })),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("DELETE", "/api/folders") => match folder_target(body).and_then(|(sub, path)| remove_folder(&sub, &path)) {
            Ok(()) => json_status(200, json!({ "ok": true })),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("GET", "/api/ignore") => match read_ignore(&query_value(query, "server"), &query_value(query, "user"), &query_value(query, "path")) {
            Ok(value) => json_status(200, value),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        ("PUT", "/api/ignore") => match json_field(body, "text").and_then(|text| write_ignore(&query_value(query, "server"), &query_value(query, "user"), &query_value(query, "path"), &text)) {
            Ok(()) => json_status(200, json!({ "ok": true })),
            Err(err) => json_status(400, json!({ "error": err })),
        },
        _ => json_status(404, json!({ "error": "Not found." })),
    }
}

fn info(app: &App) -> Value {
    let subs = config::load_all().unwrap_or_default();
    let log = fs::read_to_string(config::log_path()).unwrap_or_default();
    let subscriptions: Vec<Value> = subs.iter().map(|sub| subscription_info(sub, &log)).collect();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "linux": true,
        "running": app.running.load(Ordering::SeqCst),
        "startupDisabled": !config::startup_enabled(),
        "subscriptions": subscriptions,
    })
}

fn subscription_info(sub: &Subscription, log: &str) -> Value {
    let folders: Vec<Value> = sub.folders.iter().map(|folder| folder_info(folder, log)).collect();
    json!({
        "server": sub.server_key,
        "serverUrl": sub.server,
        "user": sub.user,
        "mount": sub.mount,
        "timer": sub.timer,
        "password": !sub.password.is_empty(),
        "folders": folders,
    })
}

fn add_subscription(body: &[u8]) -> Result<Value, String> {
    let server = json_field(body, "server")?;
    let (user, password) = split_credentials(&json_field(body, "credentials")?)?;
    let server_key = config::server_key_of(&server).map_err(|err| format!("{err:#}"))?;
    config::validate_user(&user).map_err(|err| format!("{err:#}"))?;
    let server_url = config::normalize_server(&server_key).map_err(|err| format!("{err:#}"))?;
    let url = url::Url::parse(&server_url).map_err(|err| err.to_string())?;
    upload::check_credentials(&url, &user, &password).map_err(|err| format!("{err:#}"))?;
    config::save_password(&server_key, &user, &password).map_err(|err| format!("{err:#}"))?;
    Ok(json!({ "server": server_key, "user": user }))
}

fn split_credentials(raw: &str) -> Result<(String, String), String> {
    let raw = raw.trim();
    let Some((user, password)) = raw.split_once(':') else {
        return Err("Enter credentials as subscription:password.".into());
    };
    let password = password.trim();
    if user.len() != 8 || !user.bytes().all(|byte| byte.is_ascii_digit()) || password.is_empty() {
        return Err("Enter credentials as subscription:password.".into());
    }
    Ok((user.to_string(), password.to_string()))
}

fn delete_subscription(body: &[u8]) -> Result<(), String> {
    let (sub, _) = named(body)?;
    config::delete_subscription(&sub.server_key, &sub.user).map_err(|err| format!("{err:#}"))
}

fn assign_mount(body: &[u8]) -> Result<Value, String> {
    let (sub, _) = named(body)?;
    let mount = json_field(body, "mount").or_else(|_| json_field(body, "letter"))?;
    let mount = config::normalize_mount(&mount).map_err(|err| format!("{err:#}"))?;
    if mount.is_empty() {
        return Err("Enter a mount path.".into());
    }
    let used = config::used_mounts().map_err(|err| format!("{err:#}"))?;
    if used.iter().any(|have| have == &mount && have != &sub.mount) {
        return Err(format!("{mount} is already in use."));
    }
    config::save_mount(&sub.server_key, &sub.user, &mount).map_err(|err| format!("{err:#}"))?;
    host::notify_mount();
    Ok(json!({ "mount": mount }))
}

fn enable_startup() -> Result<(), String> {
    config::ensure_startup().map_err(|err| format!("{err:#}"))
}

fn save_timer(body: &[u8]) -> Result<Value, String> {
    let (sub, _) = named(body)?;
    let timer = json_field(body, "timer").unwrap_or_else(|_| String::new());
    let stored = config::save_timer(&sub.server_key, &sub.user, &timer).map_err(|err| format!("{err:#}"))?;
    host::notify_mount();
    Ok(json!({ "timer": stored }))
}

fn start_backup(app: &Arc<App>, body: &[u8]) -> Result<(), String> {
    let (cfg, _) = named(body)?;
    if cfg.password.is_empty() {
        return Err("This subscription has no password.".into());
    }
    if cfg.folders.is_empty() {
        return Err("Add a folder before backing up.".into());
    }
    if app.running.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Err("A backup is already running.".into());
    }
    let app = Arc::clone(app);
    thread::spawn(move || {
        upload::clear_cancelled();
        let run = app.begin_run();
        let sink = Arc::clone(&app);
        upload::set_progress(Some(Arc::new(move |line| sink.push_log(run, line))));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::run_backup(&cfg)));
        upload::set_progress(None);
        let ok = match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                app.push_log(run, &format!("deppo: {err:#}"));
                false
            }
            Err(_) => {
                app.push_log(run, "deppo: the backup stopped unexpectedly");
                false
            }
        };
        app.finish_run(run, ok);
    });
    Ok(())
}

impl App {
    fn begin_run(&self) -> u64 {
        let mut live = self.live.lock().unwrap_or_else(|err| err.into_inner());
        live.run += 1;
        live.lines.clear();
        live.done = None;
        live.run
    }

    fn push_log(&self, run: u64, text: &str) {
        let mut live = self.live.lock().unwrap_or_else(|err| err.into_inner());
        if live.run != run {
            return;
        }
        live.next += 1;
        let id = live.next;
        live.lines.push_back((id, run, text.to_string()));
        while live.lines.len() > 2000 {
            live.lines.pop_front();
        }
    }

    fn finish_run(&self, run: u64, ok: bool) {
        let mut live = self.live.lock().unwrap_or_else(|err| err.into_inner());
        if live.run != run {
            return;
        }
        live.next += 1;
        live.done = Some((live.next, run, ok));
        self.running.store(false, Ordering::SeqCst);
    }
}

fn folder_info(folder: &Path, log: &str) -> Value {
    let run = latest_run(log, folder);
    let cached = cache::Cache::load(folder).map(|store| store.len()).unwrap_or(0);
    json!({
        "path": folder.display().to_string(),
        "cached": cached,
        "ignoreExists": folder.join(".deppoignore").is_file(),
        "uploaded": run.counts.uploaded,
        "already": run.counts.already,
        "unchanged": run.counts.unchanged,
        "failed": run.counts.failed,
        "summary": run.summary,
        "ranAt": run.ran_at,
    })
}

fn add_folder(sub: &Subscription, raw: &str) -> Result<String, String> {
    let folder = valid_folder(raw)?;
    if sub.folders.iter().any(|have| paths_equal(have, &folder)) {
        return Err(format!("Already listed: {}", folder.display()));
    }
    let mut folders = sub.folders.clone();
    folders.push(folder.clone());
    config::save_folders(&sub.server_key, &sub.user, &folders).map_err(|err| format!("{err:#}"))?;
    host::notify_mount();
    Ok(folder.display().to_string())
}

fn remove_folder(sub: &Subscription, raw: &str) -> Result<(), String> {
    let wanted = raw.trim();
    if wanted.is_empty() {
        return Err("Enter a folder path.".into());
    }
    let mut folders = sub.folders.clone();
    let Some(index) = folders.iter().position(|folder| paths_equal(folder, Path::new(wanted))) else {
        return Err("That folder is not in the list.".into());
    };
    folders.remove(index);
    config::save_folders(&sub.server_key, &sub.user, &folders).map_err(|err| format!("{err:#}"))?;
    host::notify_mount();
    Ok(())
}

fn read_ignore(server: &str, user: &str, raw: &str) -> Result<Value, String> {
    let sub = load_sub(server, user)?;
    let folder = configured(&sub, raw)?;
    let path = folder.join(".deppoignore");
    match fs::read_to_string(&path) {
        Ok(text) => Ok(json!({ "text": text, "exists": true })),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(json!({ "text": IGNORE_TEMPLATE, "exists": false })),
        Err(err) => Err(format!("read {}: {err}", path.display())),
    }
}

fn write_ignore(server: &str, user: &str, raw: &str, text: &str) -> Result<(), String> {
    let sub = load_sub(server, user)?;
    let folder = configured(&sub, raw)?.to_path_buf();
    if !folder.is_dir() {
        return Err(format!("Not a folder: {}", folder.display()));
    }
    let path = folder.join(".deppoignore");
    let tmp = folder.join(".deppoignore.tmp");
    fs::write(&tmp, text).map_err(|err| format!("write {}: {err}", path.display()))?;
    fs::rename(&tmp, &path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("write {}: {err}", path.display())
    })
}

fn configured<'a>(sub: &'a Subscription, raw: &str) -> Result<&'a Path, String> {
    let wanted = raw.trim();
    if wanted.is_empty() {
        return Err("Enter a folder path.".into());
    }
    sub.folders.iter().find(|folder| paths_equal(folder, Path::new(wanted))).map(PathBuf::as_path).ok_or_else(|| "That folder is not in the list.".into())
}

fn named(body: &[u8]) -> Result<(Subscription, String), String> {
    let server = json_field(body, "server")?;
    let user = json_field(body, "user")?;
    let sub = load_sub(&server, &user)?;
    Ok((sub, String::new()))
}


fn folder_target(body: &[u8]) -> Result<(Subscription, String), String> {
    let server = json_field(body, "server")?;
    let user = json_field(body, "user")?;
    let path = json_field(body, "path")?;
    Ok((load_sub(&server, &user)?, path))
}

fn load_sub(server: &str, user: &str) -> Result<Subscription, String> {
    let server_key = config::server_key_of(server).map_err(|err| format!("{err:#}"))?;
    let user = user.trim();
    config::validate_user(user).map_err(|err| format!("{err:#}"))?;
    config::find(&server_key, user).map_err(|err| format!("{err:#}"))?.ok_or_else(|| "That subscription is not saved.".to_string())
}

fn valid_folder(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a folder path.".into());
    }
    let path = if trimmed.starts_with('~') {
        config::home_dir().join(trimmed.trim_start_matches('~').trim_start_matches('/'))
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_dir() {
        return Err(format!("Not a folder: {}", path.display()));
    }
    config::absolute_path(&path).map_err(|err| format!("Not a folder: {} ({err})", path.display()))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = config::absolute_path(left).unwrap_or_else(|_| left.to_path_buf());
    let right = config::absolute_path(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn latest_run(log: &str, folder: &Path) -> Run {
    let display = folder.display().to_string();
    let mut found = None;
    for line in log.lines() {
        let line = line.trim();
        let rest = strip_stamp(line);
        let ran_at = log_stamp(line).unwrap_or("").to_string();
        if rest == display {
            found = Some(Run { counts: Counts::default(), summary: "Nothing to send.".to_string(), ran_at });
        } else if let Some(summary) = rest.strip_prefix(&display).and_then(|tail| tail.strip_prefix(": ")) {
            found = Some(Run { counts: parse_counts(summary), summary: summary.to_string(), ran_at });
        }
    }
    found.unwrap_or_default()
}

fn has_stamp(bytes: &[u8]) -> bool {
    bytes.len() > 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b' '
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b' '
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[17..19].iter().all(u8::is_ascii_digit)
}

fn log_stamp(line: &str) -> Option<&str> {
    has_stamp(line.as_bytes()).then_some(&line[..19])
}

fn strip_stamp(line: &str) -> &str {
    if has_stamp(line.as_bytes()) { &line[20..] } else { line }
}

fn parse_counts(summary: &str) -> Counts {
    let mut counts = Counts::default();
    for part in summary.split(", ") {
        let Some(number) = part.split_whitespace().last().and_then(|token| token.parse().ok()) else {
            continue;
        };
        if part.starts_with("uploaded ") {
            counts.uploaded = number;
        } else if part.starts_with("already ") {
            counts.already = number;
        } else if part.starts_with("unchanged ") {
            counts.unchanged = number;
        } else if part.starts_with("failed ") {
            counts.failed = number;
        }
    }
    counts
}

fn json_field(body: &[u8], name: &str) -> Result<String, String> {
    let value: Value = serde_json::from_slice(body).map_err(|_| "The request was not JSON.".to_string())?;
    match value.get(name).and_then(Value::as_str) {
        Some(text) => Ok(text.to_string()),
        None if name == "path" => Err("Enter a folder path.".into()),
        None => Err(format!("Missing {name}.")),
    }
}

fn json_status(status: u16, value: Value) -> (u16, &'static str, Vec<u8>) {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    (status, "application/json; charset=utf-8", body)
}

fn query_value(query: &str, name: &str) -> String {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        if key == name {
            return percent_encoding::percent_decode_str(value).decode_utf8_lossy().into_owned();
        }
    }
    String::new()
}

fn split_target(target: &str) -> (&str, &str) {
    target.split_once('?').unwrap_or((target, ""))
}

fn read_incoming(reader: &mut BufReader<TcpStream>) -> Result<Incoming> {
    let mut header = Vec::new();
    loop {
        if header.len() > 64 * 1024 {
            bail!("request header is too large");
        }
        let mut byte = [0];
        let read = reader.read(&mut byte).context("read request")?;
        if read == 0 {
            bail!("connection closed");
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&header);
    let mut lines = header_text.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    if method.is_empty() || target.is_empty() {
        bail!("bad request line");
    }
    let mut length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.eq_ignore_ascii_case("content-length") {
            length = value.trim().parse().unwrap_or(usize::MAX);
        }
    }
    if length > BODY_LIMIT {
        bail!("request body is too large");
    }
    let mut body = vec![0; length];
    if length > 0 {
        reader.read_exact(&mut body).context("read body")?;
    }
    Ok(Incoming { method, target, body })
}

fn write_bytes(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(header.as_bytes()).context("write response")?;
    stream.write_all(body).context("write response")?;
    stream.flush().context("write response")
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    }
}

fn serve_events(mut stream: TcpStream, app: &Arc<App>) -> Result<()> {
    stream.set_nodelay(true).ok();
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(header.as_bytes()).context("write events")?;
    watch_events(&mut stream, app)
}

fn watch_events(stream: &mut TcpStream, app: &App) -> Result<()> {
    write_ping(stream)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut next_ping = Instant::now() + PING_EVERY;
    let mut sent = 0u64;
    let mut buf = [0; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut || err.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err).context("read events"),
        }
        sent = write_pending(stream, app, sent)?;
        if Instant::now() >= next_ping {
            write_ping(stream)?;
            next_ping = Instant::now() + PING_EVERY;
        }
    }
}

fn write_pending(stream: &mut TcpStream, app: &App, sent: u64) -> Result<u64> {
    let (lines, done) = {
        let live = app.live.lock().unwrap_or_else(|err| err.into_inner());
        let lines: Vec<(u64, u64, String)> = live.lines.iter().filter(|(id, _, _)| *id > sent).cloned().collect();
        (lines, live.done.filter(|(id, _, _)| *id > sent))
    };
    let mut sent = sent;
    for (id, run, text) in lines {
        let data = serde_json::to_string(&json!({ "id": id, "run": run, "text": text })).unwrap_or_else(|_| "{}".to_string());
        write_event(stream, "log", &data)?;
        sent = id;
    }
    if let Some((id, run, ok)) = done {
        if id > sent {
            let data = serde_json::to_string(&json!({ "id": id, "run": run, "ok": ok })).unwrap_or_else(|_| "{}".to_string());
            write_event(stream, "done", &data)?;
            sent = id;
        }
    }
    Ok(sent)
}

fn write_event(stream: &mut TcpStream, event: &str, data: &str) -> Result<()> {
    write!(stream, "event: {event}\ndata: {data}\n\n").context("write event")?;
    stream.flush().context("write event")
}

fn write_ping(stream: &mut TcpStream) -> Result<()> {
    stream.write_all(b"event: ping\ndata: 1\n\n").context("write ping")?;
    stream.flush().context("write ping")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_matching_log_line_is_the_chart() {
        let folder = Path::new(r"D:\DEV");
        let extra = Path::new(r"D:\DEV-extra");
        let docs = PathBuf::from(r"C:\Users\docs");
        let log = format!(
            "2026-10-28 10:00:00 {}: uploaded 9, already on server 9, unchanged 9, failed 9\n\
             2026-10-28 11:00:00 {}: uploaded 1, already on server 2, unchanged 3, failed 4\n\
             plain line that is not a backup\n\
             2026-10-28 12:00:00 {}\n",
            extra.display(),
            folder.display(),
            docs.display()
        );
        let run = latest_run(&log, folder);
        assert_eq!(run.counts, Counts { uploaded: 1, already: 2, unchanged: 3, failed: 4 });
        assert_eq!(run.summary, "uploaded 1, already on server 2, unchanged 3, failed 4");
        assert_eq!(run.ran_at, "2026-10-28 11:00:00");
        let extra_run = latest_run(&log, extra);
        assert_eq!(extra_run.counts.uploaded, 9);
        assert_eq!(extra_run.ran_at, "2026-10-28 10:00:00");
        let empty = latest_run(&log, &docs);
        assert_eq!(empty.counts, Counts::default());
        assert_eq!(empty.summary, "Nothing to send.");
        assert_eq!(empty.ran_at, "2026-10-28 12:00:00");
        let missing = latest_run(&log, Path::new(r"C:\missing"));
        assert_eq!(missing.counts, Counts::default());
        assert!(missing.summary.is_empty());
        assert!(missing.ran_at.is_empty());
    }

    #[test]
    fn a_directory_can_be_added_and_a_file_cannot() {
        let dir = std::env::temp_dir().join(format!("deppo-auto-ui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let folder = valid_folder(&dir.display().to_string()).unwrap();
        assert!(folder.is_absolute());
        assert!(paths_equal(&folder, &dir));
        let file = dir.join("file.txt");
        fs::write(&file, b"x").unwrap();
        let err = valid_folder(&file.display().to_string()).unwrap_err();
        assert!(err.starts_with("Not a folder:"), "{err}");
        assert_eq!(valid_folder("   ").unwrap_err(), "Enter a folder path.");
        let _ = fs::remove_dir_all(&dir);
    }
}
