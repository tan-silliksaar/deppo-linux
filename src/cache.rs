use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

pub const CACHE_NAME: &str = ".deppocache";
const SAVE_EVERY: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub mtime_ms: i64,
    pub hash_b64: String,
}

pub struct Cache {
    path: PathBuf,
    entries: HashMap<String, CacheEntry>,
    dirty: usize,
}

impl Cache {
    pub fn load(folder: &Path) -> Result<Self> {
        let path = folder.join(CACHE_NAME);
        let mut entries = HashMap::new();
        if path.exists() {
            let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            for (idx, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                match parse_line(line) {
                    Some((logical, entry)) => {
                        entries.insert(logical, entry);
                    }
                    None => eprintln!("warning: skipped bad cache line {}:{}", path.display(), idx + 1),
                }
            }
        }
        Ok(Self { path, entries, dirty: 0 })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn get(&self, logical: &str) -> Option<&CacheEntry> {
        self.entries.get(logical)
    }

    pub fn record(&mut self, logical: String, mtime_ms: i64, hash_b64: String) -> Result<()> {
        self.entries.insert(logical, CacheEntry { mtime_ms, hash_b64 });
        self.dirty += 1;
        if self.dirty >= SAVE_EVERY {
            self.save()?;
        }
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        if self.dirty == 0 && self.path.exists() {
            return Ok(());
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .with_context(|| format!("write {}", tmp.display()))?;
            writeln!(file, "# deppocache v1")?;
            let mut keys: Vec<&String> = self.entries.keys().collect();
            keys.sort();
            for key in keys {
                let entry = &self.entries[key];
                writeln!(file, "{} {} {}", entry.mtime_ms, entry.hash_b64, key)?;
            }
            file.sync_all().ok();
        }
        fs::rename(&tmp, &self.path).with_context(|| format!("replace {}", self.path.display()))?;
        self.dirty = 0;
        Ok(())
    }
}

fn parse_line(line: &str) -> Option<(String, CacheEntry)> {
    let mut parts = line.splitn(3, ' ');
    let mtime_ms = parts.next()?.parse().ok()?;
    let hash_b64 = parts.next()?.to_string();
    let logical = parts.next()?.to_string();
    if logical.is_empty() || hash_b64.is_empty() {
        return None;
    }
    Some((logical, CacheEntry { mtime_ms, hash_b64 }))
}

pub fn mtime_ms(modified: SystemTime) -> i64 {
    modified.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub struct FolderLock {
    path: PathBuf,
}

impl Drop for FolderLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn acquire_lock(folder: &Path) -> Result<FolderLock> {
    let path = folder.join(".deppocache.lock");
    match create_lock(&path) {
        Ok(lock) => Ok(lock),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists && lock_is_stale(&path) => {
            let _ = fs::remove_file(&path);
            create_lock(&path).with_context(|| format!("lock {}", path.display()))
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            bail!("another deppo is already backing up {}", folder.display())
        }
        Err(err) => Err(err).with_context(|| format!("lock {}", path.display())),
    }
}

fn create_lock(path: &Path) -> io::Result<FolderLock> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(FolderLock { path: path.to_path_buf() })
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        return true;
    };
    pid != std::process::id() && !process_alive(pid)
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("deppo-auto-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut cache = Cache::load(&dir).unwrap();
        cache.record(r"C:\Users\tan\my file.txt".to_string(), 12, "abc+/==".to_string()).unwrap();
        cache.save().unwrap();
        let loaded = Cache::load(&dir).unwrap();
        let entry = loaded.get(r"C:\Users\tan\my file.txt").unwrap();
        assert_eq!(entry.mtime_ms, 12);
        assert_eq!(entry.hash_b64, "abc+/==");
        let _ = fs::remove_dir_all(&dir);
    }
}
