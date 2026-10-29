//! Subscriptions live in `~/.config/deppo/config.toml` for the real user.

use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::secret;

#[derive(Clone, Debug)]
pub struct Subscription {
    pub server_key: String,
    pub server: String,
    pub user: String,
    pub password: String,
    pub folders: Vec<PathBuf>,
    /// Local mount path for the virtual drive. Empty means not mounted.
    pub mount: String,
    /// Empty, a minute count, or a time `H:MM`.
    pub timer: String,
}

impl Subscription {
    pub fn can_mount(&self) -> bool {
        !self.password.is_empty() && !self.mount.trim().is_empty()
    }

    pub fn can_schedule(&self) -> bool {
        !self.password.is_empty() && !self.folders.is_empty() && !matches!(parse_backup_timer(&self.timer).unwrap_or(BackupTimer::Off), BackupTimer::Off)
    }
}

#[derive(Serialize, Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    subscriptions: Vec<FileSubscription>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FileSubscription {
    server: String,
    user: String,
    password: String,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    mount: String,
    #[serde(default)]
    timer: String,
}

pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("deppo");
        }
    }
    home_dir().join(".config").join("deppo")
}

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(".")
}

pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    std::env::temp_dir().join(format!("deppo-{}", users_uid()))
}

fn users_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("deppo").join("deppo.log");
        }
    }
    home_dir().join(".local").join("state").join("deppo").join("deppo.log")
}

/// Absolute path helper that works on Ubuntu 22.04's Rust 1.75 (no std::path::absolute).
pub fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub fn load_all() -> Result<Vec<Subscription>> {
    let path = config_path();
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let file: FileConfig = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let mut found = Vec::new();
    for row in file.subscriptions {
        match decode_subscription(row) {
            Ok(sub) => found.push(sub),
            Err(err) => crate::host::log_error(&format!("{err:#}")),
        }
    }
    found.sort_by(|a, b| (&a.server_key, &a.user).cmp(&(&b.server_key, &b.user)));
    Ok(found)
}

pub fn find(server_key: &str, user: &str) -> Result<Option<Subscription>> {
    Ok(load_all()?.into_iter().find(|sub| sub.server_key.eq_ignore_ascii_case(server_key) && sub.user == user))
}

pub fn save_password(server_key: &str, user: &str, password: &str) -> Result<()> {
    validate_key(server_key)?;
    validate_user(user)?;
    let mut file = read_file()?;
    let blob = secret::encrypt(server_key, user, password)?;
    let encoded = B64.encode(blob);
    if let Some(row) = file.subscriptions.iter_mut().find(|row| row.server.eq_ignore_ascii_case(server_key) && row.user == user) {
        row.password = encoded;
    } else {
        file.subscriptions.push(FileSubscription {
            server: server_key.to_string(),
            user: user.to_string(),
            password: encoded,
            folders: Vec::new(),
            mount: String::new(),
            timer: String::new(),
        });
    }
    write_file(&file)
}

pub fn save_folders(server_key: &str, user: &str, folders: &[PathBuf]) -> Result<()> {
    let mut file = read_file()?;
    let row = file.subscriptions.iter_mut().find(|row| row.server.eq_ignore_ascii_case(server_key) && row.user == user).context("subscription is not saved")?;
    row.folders = folders.iter().map(|folder| folder.display().to_string()).collect();
    write_file(&file)
}

pub fn save_mount(server_key: &str, user: &str, mount: &str) -> Result<()> {
    let mount = normalize_mount(mount)?;
    if !mount.is_empty() {
        crate::drive::ensure_empty_mount_dir(std::path::Path::new(&mount))?;
    }
    let mut file = read_file()?;
    let row = file.subscriptions.iter_mut().find(|row| row.server.eq_ignore_ascii_case(server_key) && row.user == user).context("subscription is not saved")?;
    row.mount = mount;
    write_file(&file)
}

pub fn save_timer(server_key: &str, user: &str, raw: &str) -> Result<String> {
    let timer = parse_backup_timer(raw)?;
    let stored = format_backup_timer(&timer);
    let mut file = read_file()?;
    let row = file.subscriptions.iter_mut().find(|row| row.server.eq_ignore_ascii_case(server_key) && row.user == user).context("subscription is not saved")?;
    row.timer = stored.clone();
    write_file(&file)?;
    Ok(stored)
}

pub fn delete_subscription(server_key: &str, user: &str) -> Result<()> {
    let mut file = read_file()?;
    file.subscriptions.retain(|row| !(row.server.eq_ignore_ascii_case(server_key) && row.user == user));
    write_file(&file)
}

pub fn used_mounts() -> Result<Vec<String>> {
    Ok(load_all()?.into_iter().map(|sub| sub.mount).filter(|mount| !mount.is_empty()).collect())
}

fn decode_subscription(row: FileSubscription) -> Result<Subscription> {
    let server_key = server_key_of(&row.server)?;
    let password = if row.password.is_empty() {
        String::new()
    } else {
        let blob = B64.decode(row.password.trim()).context("stored password")?;
        secret::decrypt(&server_key, &row.user, &blob).unwrap_or_default()
    };
    let timer = match parse_backup_timer(&row.timer) {
        Ok(timer) => format_backup_timer(&timer),
        Err(_) => String::new(),
    };
    Ok(Subscription {
        server_key: server_key.clone(),
        server: normalize_server(&row.server)?,
        user: row.user,
        password,
        folders: row.folders.into_iter().map(PathBuf::from).collect(),
        mount: normalize_mount(&row.mount).unwrap_or_default(),
        timer,
    })
}

fn read_file() -> Result<FileConfig> {
    let path = config_path();
    if !path.is_file() {
        return Ok(FileConfig::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn write_file(file: &FileConfig) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = config_path();
    let text = toml::to_string_pretty(file).context("encode config")?;
    let tmp = path.with_extension("toml.tmp");
    {
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        use std::io::Write;
        out.write_all(text.as_bytes())?;
        out.sync_all().ok();
    }
    fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn normalize_mount(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let path = if raw.starts_with('~') {
        let rest = raw.trim_start_matches('~').trim_start_matches('/');
        home_dir().join(rest)
    } else {
        PathBuf::from(raw)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir().context("current directory")?.join(path)
    };
    Ok(absolute.display().to_string())
}

pub fn normalize_server(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() {
        bail!("server address is empty");
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else if short_server_name(input) {
        format!("https://{input}.deppo.eu")
    } else {
        format!("https://{input}")
    };
    let mut url = Url::parse(&with_scheme).with_context(|| format!("invalid server address {input}"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("server must be http or https, not {}", url.scheme());
    }
    if url.host_str().is_none() {
        bail!("server address has no host");
    }
    url.set_username("").ok();
    url.set_password(None).ok();
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn server_key_of(input: &str) -> Result<String> {
    let url = Url::parse(&normalize_server(input)?)?;
    let host = url.host_str().context("server address has no host")?;
    let key = if let Some(rest) = host.strip_suffix(".deppo.eu") {
        rest.split('.').next().unwrap_or(rest).to_string()
    } else {
        host.to_string()
    };
    validate_key(&key)?;
    Ok(key)
}

pub fn validate_user(user: &str) -> Result<()> {
    if user.len() == 8 && user.chars().all(|c| c.is_ascii_digit()) {
        Ok(())
    } else {
        bail!("the subscription number is 8 digits");
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key == "." || key == ".." || key.contains(['\\', '/', ' ']) {
        bail!("server name {key} cannot be used");
    }
    Ok(())
}

fn short_server_name(input: &str) -> bool {
    let host = input.split('/').next().unwrap_or(input);
    let host = match host.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => host,
    };
    !host.contains('.') && !host.eq_ignore_ascii_case("localhost") && !host.is_empty()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupTimer {
    Off,
    Minutes(u32),
    Daily { hour: u8, minute: u8 },
}

pub fn parse_backup_timer(raw: &str) -> Result<BackupTimer> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(BackupTimer::Off);
    }
    if let Some(minutes) = interval_phrase(raw)? {
        return Ok(BackupTimer::Minutes(minutes));
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        let minutes: u32 = raw.parse().context("backup interval")?;
        if minutes == 0 || minutes > 24 * 60 {
            bail!("automatic backup can run at most every 24 hours (1440 minutes)");
        }
        return Ok(BackupTimer::Minutes(minutes));
    }
    let Some((hour_text, minute_text)) = raw.split_once(':') else {
        bail!("enter a number of minutes or a time such as 1:00");
    };
    if minute_text.len() != 2 || hour_text.is_empty() || hour_text.len() > 2 {
        bail!("use a 24-hour time such as 1:00 or 13:05");
    }
    if !hour_text.chars().all(|c| c.is_ascii_digit()) || !minute_text.chars().all(|c| c.is_ascii_digit()) {
        bail!("use a 24-hour time such as 1:00 or 13:05");
    }
    let hour: u8 = hour_text.parse().context("hour")?;
    let minute: u8 = minute_text.parse().context("minute")?;
    if hour > 23 || minute > 59 {
        bail!("use a 24-hour time such as 1:00 or 13:05");
    }
    Ok(BackupTimer::Daily { hour, minute })
}

fn interval_phrase(raw: &str) -> Result<Option<u32>> {
    if raw.contains(':') {
        let parts: Vec<&str> = raw.split(':').map(str::trim).collect();
        if parts.len() == 3 && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())) {
            let hours: u64 = parts[0].parse().context("hours")?;
            let mins: u64 = parts[1].parse().context("minutes")?;
            let secs: u64 = parts[2].parse().context("seconds")?;
            let seconds = hours.checked_mul(3600).and_then(|value| value.checked_add(mins.checked_mul(60)?)).and_then(|value| value.checked_add(secs)).context("backup interval")?;
            return Ok(Some(whole_minutes((seconds + 30) / 60)?));
        }
        return Ok(None);
    }
    let Some((number, rest)) = leading_number(raw) else {
        return Ok(None);
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(None);
    }
    let minutes = match rest.chars().next().unwrap_or_default().to_ascii_lowercase() {
        'h' => number * 60.0,
        'm' => number,
        's' => number / 60.0,
        _ => bail!("use hours, minutes, or seconds, such as 6 hours or 90 minutes"),
    };
    if !minutes.is_finite() || minutes < 0.0 {
        bail!("use hours, minutes, or seconds, such as 6 hours or 90 minutes");
    }
    Ok(Some(whole_minutes(minutes.round() as u64)?))
}

fn leading_number(raw: &str) -> Option<(f64, &str)> {
    let bytes = raw.as_bytes();
    let mut end = 0;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == 0 {
        return None;
    }
    if end < bytes.len() && bytes[end] == b'.' {
        let frac = end + 1;
        let mut cursor = frac;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor > frac {
            end = cursor;
        }
    }
    let number: f64 = raw[..end].parse().ok()?;
    Some((number, &raw[end..]))
}

fn whole_minutes(minutes: u64) -> Result<u32> {
    if minutes == 0 {
        bail!("automatic backup needs at least 1 minute");
    }
    if minutes > 1440 {
        bail!("automatic backup can run at most every 24 hours (1440 minutes)");
    }
    Ok(minutes as u32)
}

pub fn format_backup_timer(timer: &BackupTimer) -> String {
    match timer {
        BackupTimer::Off => String::new(),
        BackupTimer::Minutes(minutes) => minutes.to_string(),
        BackupTimer::Daily { hour, minute } => format!("{hour}:{minute:02}"),
    }
}

pub fn backup_due(timer: BackupTimer, elapsed_secs: u64, anchor_date: &str, anchor_minute: u32, last_day: &str, today: &str, now_minute: u32) -> bool {
    match timer {
        BackupTimer::Off => false,
        BackupTimer::Minutes(minutes) => elapsed_secs >= u64::from(minutes) * 60,
        BackupTimer::Daily { hour, minute } => {
            if last_day == today {
                return false;
            }
            let at = u32::from(hour) * 60 + u32::from(minute);
            if now_minute < at {
                return false;
            }
            !(anchor_date == today && anchor_minute > at)
        }
    }
}

/// Install and enable a systemd --user unit so Deppo starts at login.
pub fn ensure_startup() -> Result<()> {
    let packaged = PathBuf::from("/usr/lib/systemd/user/deppo.service");
    if !packaged.is_file() {
        let exe = std::env::current_exe().context("find deppo")?;
        let exe = fs::canonicalize(&exe).unwrap_or(exe);
        let unit_dir = home_dir().join(".config/systemd/user");
        fs::create_dir_all(&unit_dir).with_context(|| format!("create {}", unit_dir.display()))?;
        let unit = unit_dir.join("deppo.service");
        let body = format!(
            "[Unit]\nDescription=Deppo storage client\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=5\n\n\
             [Install]\nWantedBy=default.target\n",
            exe.display()
        );
        fs::write(&unit, body).with_context(|| format!("write {}", unit.display()))?;
        let reload = systemctl_user(&["daemon-reload"])?;
        if !reload.status.success() {
            bail!("systemctl --user daemon-reload failed: {}", trim_output(&reload));
        }
    }
    let enable = systemctl_user(&["enable", "--now", "deppo.service"])?;
    if !enable.status.success() {
        // Enable symlink alone is enough for login; --now needs a running user manager.
        let link = systemctl_user(&["enable", "deppo.service"])?;
        if link.status.success() {
            return Ok(());
        }
        bail!("could not enable deppo.service with systemctl --user: {}", trim_output(&link));
    }
    Ok(())
}

fn trim_output(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = stderr.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    let text = stdout.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    format!("exit {}", output.status.code().unwrap_or(1))
}

/// systemctl --user needs XDG_RUNTIME_DIR; IDE terminals often omit it.
fn prepare_user_systemd(command: &mut std::process::Command) {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        let dir = PathBuf::from(format!("/run/user/{}", users_uid()));
        if dir.is_dir() {
            command.env("XDG_RUNTIME_DIR", dir);
        }
    }
}

fn systemctl_user(args: &[&str]) -> Result<std::process::Output> {
    let mut command = std::process::Command::new("systemctl");
    command.arg("--user").args(args);
    prepare_user_systemd(&mut command);
    command.stdin(std::process::Stdio::null());
    command.output().with_context(|| format!("systemctl --user {}", args.join(" ")))
}

pub fn startup_enabled() -> bool {
    let wants = home_dir().join(".config/systemd/user/default.target.wants/deppo.service");
    if wants.is_symlink() || wants.is_file() {
        return true;
    }
    let Ok(output) = systemctl_user(&["is-enabled", "deppo.service"]) else {
        return false;
    };
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "enabled"
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_keys() {
        assert_eq!(server_key_of("h1").unwrap(), "h1");
        assert_eq!(server_key_of("https://h1.deppo.eu/").unwrap(), "h1");
        assert_eq!(server_key_of("h1.deppo.eu").unwrap(), "h1");
    }

    #[test]
    fn timer_phrases() {
        assert_eq!(parse_backup_timer("6 hours").unwrap(), BackupTimer::Minutes(360));
        assert_eq!(parse_backup_timer("90 minutes").unwrap(), BackupTimer::Minutes(90));
        assert_eq!(parse_backup_timer("1:00").unwrap(), BackupTimer::Daily { hour: 1, minute: 0 });
        assert_eq!(parse_backup_timer("").unwrap(), BackupTimer::Off);
    }
}
