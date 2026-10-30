mod cache;
mod config;
mod drive;
mod host;
mod secret;
mod ui;
mod upload;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use url::Url;

const IGNORE_TEMPLATE: &str = include_str!("default.deppoignore");

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("deppo: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn print_help() {
    println!(
        "deppo {} — Linux client for Deppo storage.\n\n\
         Usage:\n\
           deppo                     Keep running (mounts, backups)\n\
           deppo --ui | -u           Open the local page, then exit\n\
           deppo subscription add <server> <subscription:password>\n\
           deppo subscription list\n\
           deppo subscription remove <server> <subscription>\n\
           deppo folder add <server> <subscription> <path>\n\
           deppo folder list <server> <subscription>\n\
           deppo folder remove <server> <subscription> <path>\n\
           deppo mount set <server> <subscription> <path>\n\
           deppo mount clear <server> <subscription>\n\
           deppo timer set <server> <subscription> <spec>\n\
           deppo timer off <server> <subscription>\n\
           deppo backup <server> <subscription>\n\
           deppo enable-startup      Start Deppo at login (systemd --user)\n\n\
         Config: ~/.config/deppo/config.toml\n\
         One Deppo stays running per user. Closing the browser leaves it running.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        host::run_resident(false)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut rest = args.iter();
    let Some(cmd) = rest.next() else {
        print_help();
        return Ok(ExitCode::SUCCESS);
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "-u" | "--ui" | "/u" | "/U" => {
            host::run_resident(true)?;
            Ok(ExitCode::SUCCESS)
        }
        "enable-startup" => {
            config::ensure_startup()?;
            println!("Deppo will start at login for this user.");
            Ok(ExitCode::SUCCESS)
        }
        "subscription" => cmd_subscription(rest),
        "folder" => cmd_folder(rest),
        "mount" => cmd_mount(rest),
        "timer" => cmd_timer(rest),
        "backup" => cmd_backup(rest),
        other => bail!("unknown command {other}. Try deppo --help"),
    }
}

fn cmd_subscription<'a, I>(mut args: I) -> Result<ExitCode>
where
    I: Iterator<Item = &'a String>,
{
    let Some(action) = args.next() else {
        bail!("usage: deppo subscription add|list|remove ...");
    };
    match action.as_str() {
        "list" => {
            for sub in config::load_all()? {
                println!(
                    "{}\t{}\tmount={}\tfolders={}\ttimer={}",
                    sub.server_key,
                    sub.user,
                    if sub.mount.is_empty() { "-" } else { &sub.mount },
                    sub.folders.len(),
                    if sub.timer.is_empty() { "off" } else { &sub.timer }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "add" => {
            let server = args.next().context("server")?;
            let credentials = args.next().context("subscription:password")?;
            let (user, password) = split_credentials(credentials)?;
            let server_key = config::server_key_of(server)?;
            config::validate_user(&user)?;
            let server_url = config::normalize_server(&server_key)?;
            let url = Url::parse(&server_url)?;
            upload::check_credentials(&url, &user, &password)?;
            config::save_password(&server_key, &user, &password)?;
            println!("saved {server_key} {user}");
            Ok(ExitCode::SUCCESS)
        }
        "remove" => {
            let server = args.next().context("server")?;
            let user = args.next().context("subscription")?;
            let server_key = config::server_key_of(server)?;
            config::validate_user(user)?;
            config::delete_subscription(&server_key, user)?;
            println!("removed {server_key} {user}");
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown subscription action {other}"),
    }
}

fn cmd_folder<'a, I>(mut args: I) -> Result<ExitCode>
where
    I: Iterator<Item = &'a String>,
{
    let Some(action) = args.next() else {
        bail!("usage: deppo folder add|list|remove ...");
    };
    match action.as_str() {
        "list" => {
            let sub = require_sub(args.next(), args.next())?;
            for folder in &sub.folders {
                println!("{}", folder.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        "add" => {
            let server = args.next().context("server")?;
            let user = args.next().context("subscription")?;
            let path = args.next().context("path")?;
            let mut sub = require_sub(Some(server), Some(user))?;
            let folder = absolute_dir(path)?;
            if sub.folders.iter().any(|have| have == &folder) {
                bail!("already listed: {}", folder.display());
            }
            sub.folders.push(folder.clone());
            config::save_folders(&sub.server_key, &sub.user, &sub.folders)?;
            println!("{}", folder.display());
            Ok(ExitCode::SUCCESS)
        }
        "remove" => {
            let server = args.next().context("server")?;
            let user = args.next().context("subscription")?;
            let path = args.next().context("path")?;
            let mut sub = require_sub(Some(server), Some(user))?;
            let wanted = absolute_dir(path).unwrap_or_else(|_| Path::new(path).to_path_buf());
            let before = sub.folders.len();
            sub.folders.retain(|folder| folder != &wanted);
            if sub.folders.len() == before {
                bail!("that folder is not in the list");
            }
            config::save_folders(&sub.server_key, &sub.user, &sub.folders)?;
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown folder action {other}"),
    }
}

fn cmd_mount<'a, I>(mut args: I) -> Result<ExitCode>
where
    I: Iterator<Item = &'a String>,
{
    let Some(action) = args.next() else {
        bail!("usage: deppo mount set|clear ...");
    };
    match action.as_str() {
        "set" => {
            let server = args.next().context("server")?;
            let user = args.next().context("subscription")?;
            let path = args.next().context("path")?;
            let sub = require_sub(Some(server), Some(user))?;
            let mount = config::normalize_mount(path)?;
            config::save_mount(&sub.server_key, &sub.user, &mount)?;
            println!("{mount}");
            Ok(ExitCode::SUCCESS)
        }
        "clear" => {
            let sub = require_sub(args.next(), args.next())?;
            config::save_mount(&sub.server_key, &sub.user, "")?;
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown mount action {other}"),
    }
}

fn cmd_timer<'a, I>(mut args: I) -> Result<ExitCode>
where
    I: Iterator<Item = &'a String>,
{
    let Some(action) = args.next() else {
        bail!("usage: deppo timer set|off ...");
    };
    match action.as_str() {
        "set" => {
            let server = args.next().context("server")?;
            let user = args.next().context("subscription")?;
            let spec = args.next().context("spec")?;
            let sub = require_sub(Some(server), Some(user))?;
            let stored = config::save_timer(&sub.server_key, &sub.user, spec)?;
            println!("{stored}");
            Ok(ExitCode::SUCCESS)
        }
        "off" => {
            let sub = require_sub(args.next(), args.next())?;
            config::save_timer(&sub.server_key, &sub.user, "")?;
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown timer action {other}"),
    }
}

fn cmd_backup<'a, I>(mut args: I) -> Result<ExitCode>
where
    I: Iterator<Item = &'a String>,
{
    let sub = require_sub(args.next(), args.next())?;
    run_backup(&sub)?;
    Ok(ExitCode::SUCCESS)
}

fn require_sub(server: Option<&String>, user: Option<&String>) -> Result<config::Subscription> {
    let server = server.context("server")?;
    let user = user.context("subscription")?;
    let server_key = config::server_key_of(server)?;
    config::validate_user(user)?;
    config::find(&server_key, user)?.with_context(|| format!("subscription {server_key}\\{user} is not saved"))
}

fn split_credentials(raw: &str) -> Result<(String, String)> {
    let raw = raw.trim();
    let Some((user, password)) = raw.split_once(':') else {
        bail!("enter credentials as subscription:password");
    };
    let password = password.trim();
    if user.len() != 8 || !user.bytes().all(|byte| byte.is_ascii_digit()) || password.is_empty() {
        bail!("enter credentials as subscription:password");
    }
    Ok((user.to_string(), password.to_string()))
}

fn absolute_dir(raw: &str) -> Result<std::path::PathBuf> {
    let path = if raw.starts_with('~') {
        config::home_dir().join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else {
        std::path::PathBuf::from(raw)
    };
    let absolute = config::absolute_path(&path).with_context(|| format!("folder {}", path.display()))?;
    if !absolute.is_dir() {
        bail!("not a folder: {}", absolute.display());
    }
    Ok(absolute)
}

pub(crate) fn run_backup(cfg: &config::Subscription) -> Result<()> {
    if cfg.server.is_empty() || cfg.user.is_empty() || cfg.password.is_empty() {
        bail!("This subscription has no password.");
    }
    if cfg.folders.is_empty() {
        bail!("Add a folder before backing up.");
    }
    let server = Url::parse(&cfg.server).with_context(|| format!("server {}", cfg.server))?;
    upload::note(format!("Server {}", cfg.server));

    let mut created = Vec::new();
    let mut ready = Vec::new();
    for folder in &cfg.folders {
        let folder = config::absolute_path(folder).with_context(|| format!("folder {}", folder.display()))?;
        if !folder.is_dir() {
            upload::note_err(format!("skip {}: not a directory", folder.display()));
            continue;
        }
        let ignore = folder.join(".deppoignore");
        if !ignore.exists() {
            create_ignore(&ignore)?;
            created.push(ignore);
            continue;
        }
        ready.push(folder);
    }

    if !created.is_empty() {
        upload::note("Created .deppoignore and did not upload anything.");
        for path in &created {
            upload::note(format!("  {}", path.display()));
        }
        upload::note("The file uses .gitignore syntax. Review it, then run the backup again.");
        return Ok(());
    }
    if ready.is_empty() {
        bail!("none of the configured folders exist");
    }

    upload::install_cancel_handler();
    let mut log = open_run_log()?;
    let mut failed = 0u64;
    for folder in ready {
        upload::note(folder.display().to_string());
        let _lock = cache::acquire_lock(&folder)?;
        let mut store = cache::Cache::load(&folder)?;
        match upload::sync_folder(&server, &cfg.user, &cfg.password, &folder, &mut store) {
            Ok(stats) => {
                let line = folder_summary(&folder, &stats);
                upload::note(&line);
                append_conclusion(&mut log, &line)?;
                failed += stats.failed;
            }
            Err(err) => {
                let _ = store.save();
                if upload::cancelled() {
                    let line = format!("cancelled while backing up {}", folder.display());
                    upload::note_err(&line);
                    let _ = append_conclusion(&mut log, &line);
                    return Err(err);
                }
                let line = format!("{}: {err:#}", folder.display());
                upload::note_err(&line);
                append_conclusion(&mut log, &line)?;
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} file{} failed", if failed == 1 { "" } else { "s" });
    }
    Ok(())
}

fn create_ignore(path: &Path) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(IGNORE_TEMPLATE.as_bytes())?;
    Ok(())
}

fn folder_summary(folder: &Path, stats: &upload::FolderStats) -> String {
    let mut parts = Vec::new();
    if stats.uploaded > 0 {
        parts.push(format!("uploaded {}", stats.uploaded));
    }
    if stats.already > 0 {
        parts.push(format!("already on server {}", stats.already));
    }
    if stats.unchanged > 0 {
        parts.push(format!("unchanged {}", stats.unchanged));
    }
    if stats.failed > 0 {
        parts.push(format!("failed {}", stats.failed));
    }
    if parts.is_empty() {
        folder.display().to_string()
    } else {
        format!("{}: {}", folder.display(), parts.join(", "))
    }
}

fn open_run_log() -> Result<fs::File> {
    let path = config::log_path();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    Ok(file)
}

fn append_conclusion(log: &mut fs::File, line: &str) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    writeln!(log, "{secs} {line}").context("write deppo.log")?;
    log.flush().context("write deppo.log")
}
