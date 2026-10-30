//! One `deppo` stays running for the user. It owns backups, the local page,
//! and (later) FUSE mounts. A lock file under XDG_RUNTIME_DIR keeps a single
//! instance. The page port is published next to that lock.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::config::{self, Subscription};
use crate::drive;
use crate::ui;

const LOCK_NAME: &str = "deppo.lock";
const PORT_NAME: &str = "deppo.port";
static WAKE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);

pub fn log_error(message: &str) {
    eprintln!("deppo: {message}");
    let path = config::log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", local_stamp());
    }
}

fn local_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{secs}")
}

struct HostLock {
    _file: File,
}

fn lock_path() -> PathBuf {
    config::runtime_dir().join(LOCK_NAME)
}

fn port_path() -> PathBuf {
    config::runtime_dir().join(PORT_NAME)
}

fn try_acquire() -> Result<Option<HostLock>> {
    let dir = config::runtime_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = lock_path();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    let ok = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ok != 0 {
        return Ok(None);
    }
    let _ = file.set_len(0);
    let _ = writeln!(file, "{}", std::process::id());
    let _ = file.flush();
    Ok(Some(HostLock { _file: file }))
}

fn publish_port(port: u16) -> Result<()> {
    let path = port_path();
    fs::write(&path, format!("{port}\n")).with_context(|| format!("write {}", path.display()))
}

fn read_port() -> Option<u16> {
    let text = fs::read_to_string(port_path()).ok()?;
    text.trim().parse().ok()
}

fn wait_for_port() -> Option<u16> {
    for _ in 0..50 {
        if let Some(port) = read_port() {
            return Some(port);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Stay running until the process is stopped. Another launch without `--ui`
/// prints a message and returns. `--ui` opens the browser and always returns:
/// if nothing is running yet, a background `deppo` is started first.
pub fn run_resident(open_ui: bool) -> Result<()> {
    if open_ui {
        return open_ui_and_exit();
    }

    let Some(_lock) = try_acquire()? else {
        println!("Deppo is already running.");
        return Ok(());
    };

    let _ = ctrlc::set_handler(|| {
        STOP.store(true, Ordering::SeqCst);
        WAKE.store(true, Ordering::SeqCst);
    });

    let mut page: Option<u16> = None;
    let mut mounted = Vec::new();
    let mut reported = Vec::new();
    let mut clocks = Vec::new();
    loop {
        if STOP.load(Ordering::SeqCst) {
            drive::unmount_all();
            return Ok(());
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if page.is_none() {
                match ui::start() {
                    Ok(url) => {
                        if let Some(port) = url.rsplit_once(':').and_then(|(_, text)| text.trim_end_matches('/').parse().ok()) {
                            if let Err(err) = publish_port(port) {
                                log_error(&format!("{err:#}"));
                            } else {
                                page = Some(port);
                            }
                        }
                    }
                    Err(err) => log_error(&format!("{err:#}")),
                }
            }
            refresh_mounts(&mut mounted, &mut reported);
            run_due_backups(&mut clocks);
        }));
        if STOP.load(Ordering::SeqCst) {
            drive::unmount_all();
            return Ok(());
        }
        let wake = WAKE.swap(false, Ordering::SeqCst);
        std::thread::sleep(if page.is_none() || wake {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(30)
        });
    }
}

fn open_ui_and_exit() -> Result<()> {
    if try_acquire()?.is_none() {
        return open_published_page();
    }
    // Nobody was running. Drop the trial lock (RAII) and start one resident.
    let exe = std::env::current_exe().context("current executable")?;
    let mut command = Command::new(&exe);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // Detach so the child is not tied to this terminal and is the sole host.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("start background {}", exe.display()))?;
    open_published_page()
}

fn open_published_page() -> Result<()> {
    let Some(port) = wait_for_port() else {
        bail!("Deppo did not publish a local page in time");
    };
    let url = format!("http://127.0.0.1:{port}/");
    println!("{url}");
    ui::open_browser(&url);
    Ok(())
}
struct Clock {
    id: String,
    spec: String,
    anchor: Instant,
    anchor_date: String,
    anchor_minute: u32,
    last_interval: Option<Instant>,
    last_day: String,
}

fn run_due_backups(clocks: &mut Vec<Clock>) {
    let Ok(subs) = config::load_all() else {
        return;
    };
    let now = local_hm();
    let today = now.0;
    let now_minute = now.1;
    for sub in subs {
        if !sub.can_schedule() {
            continue;
        }
        let spec = sub.timer.trim().to_string();
        let Ok(timer) = config::parse_backup_timer(&spec) else {
            continue;
        };
        let id = format!("{}/{}", sub.server_key, sub.user);
        if !clocks.iter().any(|clock| clock.id == id) {
            clocks.push(Clock {
                id: id.clone(),
                spec: String::new(),
                anchor: Instant::now(),
                anchor_date: today.clone(),
                anchor_minute: now_minute,
                last_interval: None,
                last_day: String::new(),
            });
        }
        let Some(clock) = clocks.iter_mut().find(|clock| clock.id == id) else {
            continue;
        };
        if clock.spec != spec {
            clock.spec = spec;
            clock.anchor = Instant::now();
            clock.anchor_date = today.clone();
            clock.anchor_minute = now_minute;
            clock.last_interval = None;
            clock.last_day.clear();
        }
        let elapsed = clock.last_interval.unwrap_or(clock.anchor).elapsed().as_secs();
        if !config::backup_due(timer, elapsed, &clock.anchor_date, clock.anchor_minute, &clock.last_day, &today, now_minute) {
            continue;
        }
        match timer {
            config::BackupTimer::Minutes(_) => clock.last_interval = Some(Instant::now()),
            config::BackupTimer::Daily { .. } => clock.last_day = today.clone(),
            config::BackupTimer::Off => {}
        }
        let label = format!("{} {}", sub.server_key, sub.user);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::run_backup(&sub)));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(err)) => log_error(&format!("automatic backup {label}: {err:#}")),
            Err(_) => log_error(&format!("automatic backup {label} stopped unexpectedly")),
        }
    }
}

fn local_hm() -> (String, u32) {
    // Prefer localtime via libc.
    unsafe {
        let mut now = 0;
        libc::time(&mut now);
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return ("1970-01-01".into(), 0);
        }
        let tm = *tm;
        let date = format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday);
        let minute = (tm.tm_hour as u32) * 60 + (tm.tm_min as u32);
        (date, minute)
    }
}

fn refresh_mounts(mounted: &mut Vec<String>, reported: &mut Vec<String>) {
    let subs = match config::load_all() {
        Ok(subs) => subs,
        Err(err) => {
            let message = format!("{err:#}");
            if !reported.iter().any(|seen| seen == &message) {
                log_error(&message);
                reported.push(message);
            }
            return;
        }
    };
    let desired: Vec<Subscription> = subs.into_iter().filter(Subscription::can_mount).collect();
    let wanted: Vec<String> = desired.iter().map(|sub| sub.mount.clone()).collect();
    mounted.retain(|path| {
        if wanted.iter().any(|have| have == path) {
            true
        } else {
            drive::unmount(path);
            false
        }
    });
    reported.retain(|path| wanted.iter().any(|have| have == path) && !mounted.iter().any(|have| have == path));
    for sub in desired {
        let path = sub.mount.clone();
        if mounted.iter().any(|have| have == &path) || reported.iter().any(|have| have == &path) {
            continue;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drive::mount(&sub)));
        match outcome {
            Ok(Ok(())) => mounted.push(path),
            Ok(Err(err)) => {
                log_error(&format!("{path} ({} {}): {err:#}", sub.server_key, sub.user));
                reported.push(path);
            }
            Err(_) => {
                log_error(&format!("{path} ({} {}): the drive stopped unexpectedly", sub.server_key, sub.user));
                reported.push(path);
            }
        }
    }
}

pub fn notify_mount() {
    WAKE.store(true, Ordering::SeqCst);
}
