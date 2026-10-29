//! FUSE mount whose files are the current versions on a Deppo subscription.
//! Listings send `X-Current-Only`.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyWrite, Request, TimeOrNow,
};
use url::Url;

use crate::config::Subscription;
use crate::host;
use crate::upload::{self, HttpReply};

const TTL: Duration = Duration::from_secs(1);
const MAX_FILE: usize = 512 * 1024 * 1024;
const ROOT: u64 = 1;
/// Mode for the local mount directory while FUSE is not covering it.
/// No write bit, so copies into the path cannot pile up offline.
const IDLE_MOUNT_MODE: u32 = 0o555;
/// fusermount3 requires a writable mountpoint; restore this only for the mount call.
const ACTIVE_MOUNT_MODE: u32 = 0o755;

static NEXT_INO: AtomicU64 = AtomicU64::new(2);
static SESSIONS: Mutex<Vec<(String, fuser::BackgroundSession)>> = Mutex::new(Vec::new());

struct Backend {
    server: Url,
    user: String,
    password: String,
    cache: Mutex<HashMap<String, (Instant, Vec<Child>)>>,
    inodes: Mutex<HashMap<u64, Inode>>,
    by_path: Mutex<HashMap<String, u64>>,
    open: Mutex<HashMap<u64, OpenFile>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

#[derive(Clone)]
struct Child {
    name: String,
    is_dir: bool,
    size: u64,
    mtime_ms: i64,
}

#[derive(Clone)]
struct Inode {
    path: String,
    is_dir: bool,
    size: u64,
    mtime_ms: i64,
}

struct OpenFile {
    path: String,
    is_dir: bool,
    data: Option<Vec<u8>>,
    dirty: bool,
    size: u64,
}

struct DeppoFs {
    backend: Arc<Backend>,
}

pub fn ensure_empty_mount_dir(mount_path: &Path) -> Result<()> {
    clear_stale_fuse(mount_path);
    match fs::metadata(mount_path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => bail!("{} exists and is not a directory", mount_path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(mount_path).with_context(|| format!("create {}", mount_path.display()))?;
        }
        Err(err) => bail!("could not use {}: {err}", mount_path.display()),
    }
    match fs::read_dir(mount_path) {
        Ok(entries) => {
            if entries.filter_map(Result::ok).next().is_some() {
                bail!("{} must be an empty directory", mount_path.display());
            }
        }
        Err(err) => bail!("could not read {}: {err}", mount_path.display()),
    }
    lock_mount_dir(mount_path)
}

/// Drop a leftover FUSE mount (for example after Deppo was killed) so the path can be reused.
fn clear_stale_fuse(mount_path: &Path) {
    let not_connected = match fs::metadata(mount_path) {
        Err(err) => matches!(err.raw_os_error(), Some(libc::ENOTCONN) | Some(libc::ESTALE)),
        Ok(_) => false,
    };
    let orphan = fuse_mounted_here(mount_path) && !session_owns(mount_path);
    if !not_connected && !orphan {
        return;
    }
    let path = mount_path.display().to_string();
    for bin in ["fusermount3", "fusermount"] {
        let _ = std::process::Command::new(bin)
            .args(["-u", &path])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn session_owns(mount_path: &Path) -> bool {
    let want = mount_path.display().to_string();
    SESSIONS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .iter()
        .any(|(path, _)| path == &want)
}

fn fuse_mounted_here(mount_path: &Path) -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    let want = mount_path.display().to_string();
    text.lines().any(|line| {
        let Some(mount) = line.split(' ').nth(4) else {
            return false;
        };
        mount == want && line.contains("fuse")
    })
}

/// Keep the local path non-writable whenever FUSE is not mounted over it.
pub fn lock_mount_dir(mount_path: &Path) -> Result<()> {
    set_mount_mode(mount_path, IDLE_MOUNT_MODE)
}

fn unlock_mount_dir(mount_path: &Path) -> Result<()> {
    set_mount_mode(mount_path, ACTIVE_MOUNT_MODE)
}

fn set_mount_mode(mount_path: &Path, mode: u32) -> Result<()> {
    if !mount_path.is_dir() {
        return Ok(());
    }
    fs::set_permissions(mount_path, fs::Permissions::from_mode(mode)).with_context(|| format!("chmod {:o} {}", mode, mount_path.display()))
}

pub fn mount(sub: &Subscription) -> Result<()> {
    let mount = sub.mount.trim();
    if mount.is_empty() {
        bail!("no mount path");
    }
    let mount_path = Path::new(mount);
    ensure_empty_mount_dir(mount_path)?;
    // fusermount3 refuses a non-writable mountpoint; unlock only for this call.
    unlock_mount_dir(mount_path)?;

    let server = Url::parse(&sub.server).with_context(|| format!("server {}", sub.server))?;
    let backend = Arc::new(Backend {
        server,
        user: sub.user.clone(),
        password: sub.password.clone(),
        cache: Mutex::new(HashMap::new()),
        inodes: Mutex::new(HashMap::from([(
            ROOT,
            Inode {
                path: "/".into(),
                is_dir: true,
                size: 0,
                mtime_ms: 0,
            },
        )])),
        by_path: Mutex::new(HashMap::from([("/".into(), ROOT)])),
        open: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(1),
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    });
    let fs = DeppoFs { backend };
    // Do not use AutoUnmount: it forces allow_other, which needs user_allow_other in fuse.conf.
    let options = vec![
        MountOption::FSName("deppo".into()),
        MountOption::Subtype("deppo".into()),
        MountOption::DefaultPermissions,
    ];
    let session = match fuser::spawn_mount2(fs, mount_path, &options) {
        Ok(session) => session,
        Err(err) => {
            let _ = lock_mount_dir(mount_path);
            return Err(err).with_context(|| format!("mount {}", mount_path.display()));
        }
    };
    SESSIONS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .push((mount.to_string(), session));
    Ok(())
}

/// Drop the FUSE session for `mount_path` and make the directory non-writable again.
pub fn unmount(mount_path: &str) {
    let mount_path = mount_path.trim();
    if mount_path.is_empty() {
        return;
    }
    {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|err| err.into_inner());
        sessions.retain(|(path, _)| path != mount_path);
    }
    let _ = lock_mount_dir(Path::new(mount_path));
}

/// Drop every FUSE session (process shutdown) and lock the local directories again.
pub fn unmount_all() {
    let paths: Vec<String> = {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|err| err.into_inner());
        sessions.drain(..).map(|(path, _session)| path).collect()
    };
    for path in paths {
        let _ = lock_mount_dir(Path::new(&path));
    }
}

impl Backend {
    fn inode(&self, ino: u64) -> Option<Inode> {
        self.inodes.lock().unwrap_or_else(|err| err.into_inner()).get(&ino).cloned()
    }

    fn ensure_child(&self, parent_path: &str, child: &Child) -> u64 {
        let path = join_path(parent_path, &child.name);
        {
            let by_path = self.by_path.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(ino) = by_path.get(&path) {
                let mut inodes = self.inodes.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(node) = inodes.get_mut(ino) {
                    node.is_dir = child.is_dir;
                    node.size = child.size;
                    node.mtime_ms = child.mtime_ms;
                }
                return *ino;
            }
        }
        let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
        self.by_path.lock().unwrap_or_else(|err| err.into_inner()).insert(path.clone(), ino);
        self.inodes.lock().unwrap_or_else(|err| err.into_inner()).insert(
            ino,
            Inode {
                path,
                is_dir: child.is_dir,
                size: child.size,
                mtime_ms: child.mtime_ms,
            },
        );
        ino
    }

    fn ensure_path(&self, path: &str, is_dir: bool, size: u64, mtime_ms: i64) -> u64 {
        {
            let by_path = self.by_path.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(ino) = by_path.get(path) {
                let mut inodes = self.inodes.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(node) = inodes.get_mut(ino) {
                    node.is_dir = is_dir;
                    node.size = size;
                    node.mtime_ms = mtime_ms;
                }
                return *ino;
            }
        }
        let ino = NEXT_INO.fetch_add(1, Ordering::Relaxed);
        self.by_path.lock().unwrap_or_else(|err| err.into_inner()).insert(path.to_string(), ino);
        self.inodes.lock().unwrap_or_else(|err| err.into_inner()).insert(
            ino,
            Inode {
                path: path.to_string(),
                is_dir,
                size,
                mtime_ms,
            },
        );
        ino
    }

    fn attr_of(&self, node: &Inode, ino: u64) -> FileAttr {
        FileAttr {
            ino,
            size: if node.is_dir { 0 } else { node.size },
            blocks: if node.is_dir { 0 } else { node.size.saturating_add(511) / 512 },
            atime: time_from_ms(node.mtime_ms),
            mtime: time_from_ms(node.mtime_ms),
            ctime: time_from_ms(node.mtime_ms),
            crtime: time_from_ms(node.mtime_ms),
            kind: if node.is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if node.is_dir { 0o755 } else { 0o644 },
            nlink: if node.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn lookup_meta(&self, path: &str) -> Result<Option<(bool, u64, i64)>, libc::c_int> {
        if path == "/" {
            return Ok(Some((true, 0, 0)));
        }
        let parent = parent_path(path);
        let name = path.rsplit('/').next().unwrap_or("");
        let children = self.children(&parent)?;
        Ok(children.into_iter().find(|child| child.name == name).map(|child| (child.is_dir, child.size, child.mtime_ms)))
    }

    fn children(&self, dir: &str) -> Result<Vec<Child>, libc::c_int> {
        if let Ok(cache) = self.cache.lock() {
            if let Some((at, children)) = cache.get(dir) {
                if at.elapsed() < Duration::from_secs(2) {
                    return Ok(children.clone());
                }
            }
        }
        let reply = self.call("GET", dir, true, &[("X-Current-Only", "1")], &[], 8 * 1024 * 1024)?;
        if reply.status == 404 {
            return Err(libc::ENOENT);
        }
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        let children = parse_children(&reply.body).map_err(|_| libc::EIO)?;
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(dir.to_string(), (Instant::now(), children.clone()));
        }
        Ok(children)
    }

    fn load_bytes(&self, path: &str) -> Result<Vec<u8>, libc::c_int> {
        let reply = self.call("GET", path, false, &[], &[], MAX_FILE)?;
        if reply.status == 404 {
            return Err(libc::ENOENT);
        }
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        Ok(reply.body)
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), libc::c_int> {
        let reply = self.call("PUT", path, false, &[], bytes, 64 * 1024)?;
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        self.invalidate();
        Ok(())
    }

    fn mkcol(&self, path: &str) -> Result<(), libc::c_int> {
        let reply = self.call("MKCOL", path, true, &[], &[], 64 * 1024)?;
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        self.invalidate();
        Ok(())
    }

    fn remove(&self, path: &str, is_dir: bool) -> Result<(), libc::c_int> {
        if path == "/" {
            return Err(libc::EPERM);
        }
        // Folder DELETE removes the whole tree on the server. Do not list children
        // here — that is what makes clients delete entry-by-entry (see WinFsp CanDelete).
        let extra: &[(&str, &str)] = if is_dir { &[("Depth", "infinity")] } else { &[] };
        let reply = self.call("DELETE", path, is_dir, extra, &[], 64 * 1024)?;
        if reply.status == 404 {
            self.invalidate();
            return Ok(());
        }
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        self.invalidate();
        Ok(())
    }

    fn move_path(&self, from: &str, to: &str, is_dir: bool) -> Result<(), libc::c_int> {
        let destination = if is_dir { format!("{to}/") } else { to.to_string() };
        let reply = self.call("MOVE", from, is_dir, &[("Destination", &destination), ("Overwrite", "T")], &[], 64 * 1024)?;
        if !(200..300).contains(&reply.status) {
            return Err(http_errno(reply.status));
        }
        self.invalidate();
        Ok(())
    }

    fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    fn call(&self, method: &str, path: &str, dir: bool, extra: &[(&str, &str)], body: &[u8], max_body: usize) -> Result<HttpReply, libc::c_int> {
        let mut target = upload::request_target(&self.server, path);
        if dir && target != "/" && !target.ends_with('/') {
            target.push('/');
        }
        match upload::http_exchange(&self.server, &self.user, &self.password, method, &target, extra, body, max_body) {
            Ok(reply) => Ok(reply),
            Err(err) => {
                host::log_error(&format!("{method} {path}: {err:#}"));
                Err(libc::EIO)
            }
        }
    }

    fn forget_path(&self, path: &str) {
        if let Some(ino) = self.by_path.lock().unwrap_or_else(|err| err.into_inner()).remove(path) {
            self.inodes.lock().unwrap_or_else(|err| err.into_inner()).remove(&ino);
        }
    }

    fn forget_tree(&self, path: &str) {
        let path = path.trim_end_matches('/');
        let prefix = format!("{path}/");
        let mut by_path = self.by_path.lock().unwrap_or_else(|err| err.into_inner());
        let mut inodes = self.inodes.lock().unwrap_or_else(|err| err.into_inner());
        let stale: Vec<String> = by_path
            .keys()
            .filter(|key| key.as_str() == path || key.starts_with(&prefix))
            .cloned()
            .collect();
        for key in stale {
            if let Some(ino) = by_path.remove(&key) {
                inodes.remove(&ino);
            }
        }
    }
}

impl Filesystem for DeppoFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.backend.children(&parent_node.path) {
            Ok(children) => {
                if let Some(child) = children.into_iter().find(|child| child.name == name) {
                    let ino = self.backend.ensure_child(&parent_node.path, &child);
                    let node = self.backend.inode(ino).unwrap();
                    reply.entry(&TTL, &self.backend.attr_of(&node, ino), 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            Err(err) => reply.error(err),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(node) = self.backend.inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        reply.attr(&TTL, &self.backend.attr_of(&node, ino));
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let Some(mut node) = self.backend.inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if let Some(size) = size {
            if node.is_dir {
                reply.error(libc::EISDIR);
                return;
            }
            let result = (|| -> Result<(), libc::c_int> {
                if let Some(fh) = fh {
                    let mut open = self.backend.open.lock().unwrap_or_else(|err| err.into_inner());
                    if let Some(file) = open.get_mut(&fh) {
                        let data = file.data.get_or_insert_with(Vec::new);
                        data.resize(size.min(MAX_FILE as u64) as usize, 0);
                        file.size = size;
                        file.dirty = true;
                        return Ok(());
                    }
                }
                let mut bytes = self.backend.load_bytes(&node.path)?;
                bytes.resize(size.min(MAX_FILE as u64) as usize, 0);
                self.backend.put(&node.path, &bytes)?;
                Ok(())
            })();
            if let Err(err) = result {
                reply.error(err);
                return;
            }
            node.size = size;
            self.backend.ensure_path(&node.path, false, size, node.mtime_ms);
        }
        let node = self.backend.inode(ino).unwrap_or(node);
        reply.attr(&TTL, &self.backend.attr_of(&node, ino));
    }

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let path = join_path(&parent_node.path, name);
        if let Err(err) = self.backend.mkcol(&path) {
            reply.error(err);
            return;
        }
        let ino = self.backend.ensure_path(&path, true, 0, now_ms());
        let node = self.backend.inode(ino).unwrap();
        reply.entry(&TTL, &self.backend.attr_of(&node, ino), 0);
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let path = join_path(&parent_node.path, name);
        if let Err(err) = self.backend.put(&path, &[]) {
            reply.error(err);
            return;
        }
        let ino = self.backend.ensure_path(&path, false, 0, now_ms());
        let fh = self.backend.next_fh.fetch_add(1, Ordering::Relaxed);
        self.backend.open.lock().unwrap_or_else(|err| err.into_inner()).insert(
            fh,
            OpenFile {
                path: path.clone(),
                is_dir: false,
                data: Some(Vec::new()),
                dirty: false,
                size: 0,
            },
        );
        let node = self.backend.inode(ino).unwrap();
        reply.created(&TTL, &self.backend.attr_of(&node, ino), 0, fh, 0);
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let path = join_path(&parent_node.path, name);
        if let Err(err) = self.backend.remove(&path, false) {
            reply.error(err);
            return;
        }
        self.backend.forget_path(&path);
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        // Allow non-empty folders. Returning ENOTEMPTY makes file managers list every
        // child and delete them one by one. The server DELETE removes the whole tree.
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let path = join_path(&parent_node.path, name);
        if let Err(err) = self.backend.remove(&path, true) {
            reply.error(err);
            return;
        }
        self.backend.forget_tree(&path);
        reply.ok();
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let Some(parent_node) = self.backend.inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(new_parent) = self.backend.inode(newparent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let Some(newname) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let from = join_path(&parent_node.path, name);
        let to = join_path(&new_parent.path, newname);
        let is_dir = match self.backend.lookup_meta(&from) {
            Ok(Some((is_dir, _, _))) => is_dir,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(err) => {
                reply.error(err);
                return;
            }
        };
        if let Err(err) = self.backend.move_path(&from, &to, is_dir) {
            reply.error(err);
            return;
        }
        self.backend.forget_path(&from);
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        let Some(node) = self.backend.inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if node.is_dir {
            reply.error(libc::EISDIR);
            return;
        }
        let fh = self.backend.next_fh.fetch_add(1, Ordering::Relaxed);
        self.backend.open.lock().unwrap_or_else(|err| err.into_inner()).insert(
            fh,
            OpenFile {
                path: node.path.clone(),
                is_dir: false,
                data: None,
                dirty: false,
                size: node.size,
            },
        );
        reply.opened(fh, 0);
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let mut open = self.backend.open.lock().unwrap_or_else(|err| err.into_inner());
        let Some(file) = open.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        if file.data.is_none() {
            match self.backend.load_bytes(&file.path) {
                Ok(bytes) => {
                    file.size = bytes.len() as u64;
                    file.data = Some(bytes);
                    if let Some(node) = self.backend.inode(ino).as_ref() {
                        self.backend.ensure_path(&node.path, false, file.size, node.mtime_ms);
                    }
                }
                Err(err) => {
                    reply.error(err);
                    return;
                }
            }
        }
        let data = file.data.as_ref().unwrap();
        if offset < 0 || offset as u64 >= data.len() as u64 {
            reply.data(&[]);
            return;
        }
        let start = offset as usize;
        let end = (start + size as usize).min(data.len());
        reply.data(&data[start..end]);
    }

    fn write(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock: Option<u64>, reply: ReplyWrite) {
        let mut open = self.backend.open.lock().unwrap_or_else(|err| err.into_inner());
        let Some(file) = open.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        if file.is_dir {
            reply.error(libc::EISDIR);
            return;
        }
        if file.data.is_none() {
            match self.backend.load_bytes(&file.path) {
                Ok(bytes) => file.data = Some(bytes),
                Err(libc::ENOENT) => file.data = Some(Vec::new()),
                Err(err) => {
                    reply.error(err);
                    return;
                }
            }
        }
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        let body = file.data.get_or_insert_with(Vec::new);
        let start = offset as usize;
        let end = start + data.len();
        if end > MAX_FILE {
            reply.error(libc::EFBIG);
            return;
        }
        if body.len() < end {
            body.resize(end, 0);
        }
        body[start..end].copy_from_slice(data);
        file.size = body.len() as u64;
        file.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if let Err(err) = flush_fh(&self.backend, fh) {
            reply.error(err);
        } else {
            reply.ok();
        }
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        let _ = flush_fh(&self.backend, fh);
        self.backend.open.lock().unwrap_or_else(|err| err.into_inner()).remove(&fh);
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        let Some(node) = self.backend.inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !node.is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }
        let fh = self.backend.next_fh.fetch_add(1, Ordering::Relaxed);
        self.backend.open.lock().unwrap_or_else(|err| err.into_inner()).insert(
            fh,
            OpenFile {
                path: node.path,
                is_dir: true,
                data: None,
                dirty: false,
                size: 0,
            },
        );
        reply.opened(fh, 0);
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(node) = self.backend.inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        if !node.is_dir {
            reply.error(libc::ENOTDIR);
            return;
        }
        let children = match self.backend.children(&node.path) {
            Ok(children) => children,
            Err(err) => {
                reply.error(err);
                return;
            }
        };
        let mut entries = vec![(ino, FileType::Directory, ".".to_string()), (parent_ino(&self.backend, &node.path), FileType::Directory, "..".to_string())];
        for child in children {
            let child_ino = self.backend.ensure_child(&node.path, &child);
            entries.push((child_ino, if child.is_dir { FileType::Directory } else { FileType::RegularFile }, child.name));
        }
        for (index, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (index + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, reply: ReplyEmpty) {
        self.backend.open.lock().unwrap_or_else(|err| err.into_inner()).remove(&fh);
        reply.ok();
    }
}

fn flush_fh(backend: &Backend, fh: u64) -> Result<(), libc::c_int> {
    let mut open = backend.open.lock().unwrap_or_else(|err| err.into_inner());
    let Some(file) = open.get_mut(&fh) else {
        return Ok(());
    };
    if !file.dirty {
        return Ok(());
    }
    let bytes = file.data.clone().unwrap_or_default();
    backend.put(&file.path, &bytes)?;
    file.dirty = false;
    Ok(())
}

fn parent_ino(backend: &Backend, path: &str) -> u64 {
    if path == "/" {
        return ROOT;
    }
    let parent = parent_path(path);
    *backend.by_path.lock().unwrap_or_else(|err| err.into_inner()).get(&parent).unwrap_or(&ROOT)
}

fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        None | Some(("", _)) => "/".into(),
        Some((parent, _)) if parent.is_empty() => "/".into(),
        Some((parent, _)) => parent.to_string(),
    }
}

fn parse_children(body: &[u8]) -> Result<Vec<Child>> {
    let value: serde_json::Value = serde_json::from_slice(body).context("listing")?;
    let rows = value.as_array().context("listing")?;
    let mut children = Vec::new();
    for row in rows {
        let Some(row) = row.as_array() else { continue };
        let Some(name) = row.first().and_then(|item| item.as_str()) else { continue };
        let is_dir = name.ends_with('/');
        let name = name.trim_matches('/');
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let mtime_ms = row.get(1).and_then(|item| item.as_i64()).unwrap_or(0);
        let size = row.get(2).and_then(|item| item.as_u64()).unwrap_or(0);
        children.push(Child {
            name: name.to_string(),
            is_dir,
            size,
            mtime_ms,
        });
    }
    Ok(children)
}

fn http_errno(status: u16) -> libc::c_int {
    match status {
        401 | 403 => libc::EACCES,
        404 => libc::ENOENT,
        409 | 412 => libc::EEXIST,
        _ => libc::EIO,
    }
}

fn time_from_ms(ms: i64) -> SystemTime {
    if ms <= 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
