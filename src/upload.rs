use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};
use url::Url;

use crate::cache::{self, Cache};

const WORKERS: usize = 5;
const IO_TIMEOUT: Duration = Duration::from_secs(600);
const READ_POLL: Duration = Duration::from_secs(1);

static CANCELLED: AtomicBool = AtomicBool::new(false);
static CANCEL_INSTALLED: AtomicBool = AtomicBool::new(false);
static PROGRESS: Mutex<Option<Arc<dyn Fn(&str) + Send + Sync>>> = Mutex::new(None);

pub fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

pub fn clear_cancelled() {
    CANCELLED.store(false, Ordering::SeqCst);
}

pub fn set_progress(sink: Option<Arc<dyn Fn(&str) + Send + Sync>>) {
    *PROGRESS.lock().unwrap_or_else(|err| err.into_inner()) = sink;
}

pub(crate) fn note(line: impl AsRef<str>) {
    let line = line.as_ref();
    println!("{line}");
    emit_progress(line);
}

pub(crate) fn note_err(line: impl AsRef<str>) {
    let line = line.as_ref();
    eprintln!("{line}");
    emit_progress(line);
}

fn emit_progress(line: &str) {
    let guard = PROGRESS.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(sink) = guard.as_ref() {
        sink(line);
    }
}

pub fn install_cancel_handler() {
    if CANCEL_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    ctrlc::set_handler(|| {
        CANCELLED.store(true, Ordering::SeqCst);
    })
    .ok();
}

pub fn logical_upload_path(path: &Path) -> String {
    logical_from_display(&path.to_string_lossy())
}

pub fn logical_from_display(raw: &str) -> String {
    let mut text = raw.replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/") {
        if let Some(unc) = rest.strip_prefix("UNC/") {
            text = format!("/{unc}");
        } else {
            text = format!("/{rest}");
        }
    }
    text = text.replace(':', "");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    while text.contains("//") {
        text = text.replace("//", "/");
    }
    text
}

pub fn request_target(server: &Url, logical: &str) -> String {
    let prefix = server.path().trim_end_matches('/');
    let logical = if logical.starts_with('/') { logical } else { return encode_path(&format!("/{logical}")); };
    encode_path(&format!("{prefix}{logical}"))
}

const PATH_SEGMENT: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

fn encode_path(path: &str) -> String {
    let mut out = String::from("/");
    let mut first = true;
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if !first {
            out.push('/');
        }
        first = false;
        let encoded: String = utf8_percent_encode(segment, PATH_SEGMENT).collect();
        out.push_str(&encoded);
    }
    out
}

pub fn basic_authorization(user: &str, password: &str) -> String {
    format!("Basic {}", B64.encode(format!("{user}:{password}")))
}

pub fn host_header(url: &Url) -> String {
    let host = match url.host() {
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

struct Job {
    local: std::path::PathBuf,
    logical: String,
    mtime_ms: i64,
}

struct Done {
    logical: String,
    mtime_ms: i64,
    hash_b64: String,
    result: PutResult,
}

enum PutResult {
    Uploaded,
    AlreadyThere,
    Failed { fatal: bool, message: String },
}

pub struct FolderStats {
    pub uploaded: u64,
    pub already: u64,
    pub unchanged: u64,
    pub failed: u64,
}

pub fn sync_folder(server: &Url, user: &str, password: &str, folder: &Path, cache: &mut Cache) -> Result<FolderStats> {
    let matcher = ignore_matcher(folder)?;
    let mut walker = ignore::WalkBuilder::new(folder);
    walker
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            !matcher.matched(entry.path(), is_dir).is_ignore()
        });
    let walk = walker.build();

    let (job_tx, job_rx) = std::sync::mpsc::sync_channel::<Job>(WORKERS * 4);
    let job_rx = Arc::new(Mutex::new(job_rx));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<Done>();
    let authorization = basic_authorization(user, password);
    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let job_rx = Arc::clone(&job_rx);
        let done_tx = done_tx.clone();
        let server = server.clone();
        let authorization = authorization.clone();
        handles.push(thread::spawn(move || worker(job_rx, done_tx, server, authorization)));
    }
    drop(done_tx);

    let mut stats = FolderStats { uploaded: 0, already: 0, unchanged: 0, failed: 0 };
    let mut pending = 0u64;
    let mut fatal = false;
    let mut walk = walk.into_iter();
    let mut walk_done = false;

    loop {
        if cancelled() {
            fatal = true;
            walk_done = true;
        }
        if !walk_done && !fatal && pending < (WORKERS * 4) as u64 {
            match walk.next() {
                Some(Ok(entry)) => {
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        continue;
                    }
                    let logical = logical_upload_path(entry.path());
                    let meta = match entry.metadata() {
                        Ok(meta) => meta,
                        Err(err) => {
                            note_err(format!("skip {}: {err}", entry.path().display()));
                            stats.failed += 1;
                            continue;
                        }
                    };
                    let mtime_ms = meta.modified().map(cache::mtime_ms).unwrap_or(0);
                    if cache.get(&logical).is_some_and(|cached| cached.mtime_ms == mtime_ms) {
                        stats.unchanged += 1;
                        continue;
                    }
                    match job_tx.try_send(Job {
                        local: entry.path().to_path_buf(),
                        logical,
                        mtime_ms,
                    }) {
                        Ok(()) => pending += 1,
                        Err(std::sync::mpsc::TrySendError::Full(job)) => {
                            apply_done(cache, &mut stats, done_rx.recv().context("upload worker stopped")?)?;
                            pending -= 1;
                            job_tx.send(job).context("upload worker stopped")?;
                            pending += 1;
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => bail!("upload workers stopped"),
                    }
                    continue;
                }
                Some(Err(err)) => {
                    note_err(format!("skip: {err}"));
                    stats.failed += 1;
                    continue;
                }
                None => walk_done = true,
            }
        }

        if pending == 0 && walk_done {
            break;
        }
        let done = done_rx.recv().context("upload worker stopped")?;
        pending -= 1;
        if apply_done(cache, &mut stats, done)? {
            fatal = true;
            walk_done = true;
        }
    }
    drop(job_tx);
    for handle in handles {
        let _ = handle.join();
    }
    cache.save()?;
    if cancelled() {
        bail!("cancelled");
    }
    Ok(stats)
}

fn apply_done(cache: &mut Cache, stats: &mut FolderStats, done: Done) -> Result<bool> {
    match done.result {
        PutResult::Uploaded => {
            note(format!("put {}", done.logical));
            cache.record(done.logical, done.mtime_ms, done.hash_b64)?;
            stats.uploaded += 1;
            Ok(false)
        }
        PutResult::AlreadyThere => {
            note(format!("have {}", done.logical));
            cache.record(done.logical, done.mtime_ms, done.hash_b64)?;
            stats.already += 1;
            Ok(false)
        }
        PutResult::Failed { fatal, message } => {
            note_err(format!("fail {}: {message}", done.logical));
            stats.failed += 1;
            Ok(fatal)
        }
    }
}

fn ignore_matcher(folder: &Path) -> Result<ignore::gitignore::Gitignore> {
    let path = folder.join(".deppoignore");
    let mut builder = ignore::gitignore::GitignoreBuilder::new(folder);
    if let Some(err) = builder.add(&path) {
        return Err(anyhow!(err)).with_context(|| format!("read {}", path.display()));
    }
    builder.build().with_context(|| format!("parse {}", path.display()))
}

fn worker(
    jobs: Arc<Mutex<std::sync::mpsc::Receiver<Job>>>,
    done: std::sync::mpsc::Sender<Done>,
    server: Url,
    authorization: String,
) {
    let mut uploader = Uploader { server, authorization, conn: None };
    loop {
        let job = {
            let rx = jobs.lock().unwrap_or_else(|err| err.into_inner());
            rx.recv()
        };
        let Ok(job) = job else { break };
        if cancelled() {
            let _ = done.send(Done {
                logical: job.logical,
                mtime_ms: job.mtime_ms,
                hash_b64: String::new(),
                result: PutResult::Failed { fatal: true, message: "cancelled".to_string() },
            });
            continue;
        }
        let result = upload_job(&mut uploader, &job);
        let (hash_b64, result) = match result {
            Ok((hash, put)) => (hash, put),
            Err(err) => (
                String::new(),
                PutResult::Failed { fatal: false, message: format!("{err:#}") },
            ),
        };
        if done
            .send(Done {
                logical: job.logical,
                mtime_ms: job.mtime_ms,
                hash_b64,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}

fn upload_job(uploader: &mut Uploader, job: &Job) -> Result<(String, PutResult)> {
    let (hash_b64, len) = hash_file(&job.local)?;
    let target = request_target(&uploader.server, &job.logical);
    let host = host_header(&uploader.server);
    let mut last_message = String::new();
    for attempt in 0..2 {
        match uploader.put(&host, &target, &hash_b64, len, &job.local) {
            Ok(PutResult::Failed { fatal: false, message }) if attempt == 0 => {
                uploader.conn = None;
                last_message = message;
                continue;
            }
            Ok(result) => return Ok((hash_b64, result)),
            Err(err) if attempt == 0 => {
                uploader.conn = None;
                last_message = format!("{err:#}");
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    Ok((
        hash_b64,
        PutResult::Failed {
            fatal: false,
            message: last_message,
        },
    ))
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let len = io::copy(&mut file, &mut hasher).with_context(|| format!("read {}", path.display()))?;
    Ok((B64.encode(hasher.finalize()), len))
}

struct Uploader {
    server: Url,
    authorization: String,
    conn: Option<BufReader<Box<dyn Io + Send>>>,
}

trait Io: Read + Write {}
impl<T: Read + Write + ?Sized> Io for T {}

impl Uploader {
    fn put(&mut self, host: &str, target: &str, hash_b64: &str, len: u64, path: &Path) -> Result<PutResult> {
        if self.conn.is_none() {
            self.conn = Some(connect(&self.server)?);
        }
        let outcome = (|| {
            let stream = self.conn.as_mut().expect("connection");
            let sock = stream.get_mut();
            write!(sock, "PUT {target} HTTP/1.1\r\n")?;
            write!(sock, "Host: {host}\r\n")?;
            write!(sock, "Authorization: {}\r\n", self.authorization)?;
            write!(sock, "If-None-Match: \"{hash_b64}\"\r\n")?;
            write!(sock, "Expect: 100-continue\r\n")?;
            write!(sock, "Content-Length: {len}\r\n")?;
            write!(sock, "User-Agent: deppo/0.1\r\n")?;
            write!(sock, "Connection: keep-alive\r\n\r\n")?;
            sock.flush()?;

            let response = read_until_final(stream, |reader| {
                let mut file = File::open(path)?;
                copy_exact(&mut file, reader.get_mut(), len)?;
                reader.get_mut().flush()?;
                Ok(())
            })?;
            finish_put(stream, response)
        })();
        match outcome {
            Ok((result, close)) => {
                if close {
                    self.conn = None;
                }
                Ok(result)
            }
            Err(err) => {
                self.conn = None;
                Err(err)
            }
        }
    }
}

fn finish_put(stream: &mut BufReader<Box<dyn Io + Send>>, response: HttpResponse) -> Result<(PutResult, bool)> {
    let framed = response_is_framed(&response);
    let body = drain_body(stream, &response.headers, true)?;
    let close = !framed
        || response
            .headers
            .iter()
            .any(|(name, value)| name == "connection" && value.eq_ignore_ascii_case("close"));
    if (200..300).contains(&response.status) {
        return Ok((PutResult::Uploaded, close));
    }
    if response.status == 412 {
        return Ok((PutResult::AlreadyThere, close));
    }
    let fatal = response.status == 401 || response.status == 403;
    let detail = body.trim();
    let message = if detail.is_empty() {
        format!("HTTP {}", response.status)
    } else {
        format!("HTTP {}: {}", response.status, detail.chars().take(300).collect::<String>())
    };
    Ok((PutResult::Failed { fatal, message }, true))
}

fn response_is_framed(response: &HttpResponse) -> bool {
    if response.status == 204 || response.status == 304 {
        return true;
    }
    response.headers.iter().any(|(name, value)| {
        name == "content-length" || (name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked"))
    })
}

/// Read past one or more `100 Continue` responses. The body is sent at most once.
fn read_until_final<R: BufRead>(reader: &mut R, mut send_body: impl FnMut(&mut R) -> Result<()>) -> Result<HttpResponse> {
    let mut body_sent = false;
    for _ in 0..8 {
        let response = read_response(reader)?;
        if !(100..200).contains(&response.status) {
            return Ok(response);
        }
        drain_body(reader, &response.headers, false)?;
        if !body_sent {
            send_body(reader)?;
            body_sent = true;
        }
    }
    bail!("too many interim 100 Continue responses")
}

fn copy_exact(file: &mut File, writer: &mut impl Write, len: u64) -> Result<()> {
    let mut left = len;
    let mut buf = [0u8; 64 * 1024];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..want]).context("file changed while uploading")?;
        writer.write_all(&buf[..want])?;
        left -= want as u64;
    }
    Ok(())
}

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
}

fn read_response(reader: &mut impl BufRead) -> Result<HttpResponse> {
    let status_line = read_line_wait(reader)?;
    if status_line.is_empty() {
        bail!("connection closed before a response");
    }
    let status = parse_status(&status_line)?;
    let mut headers = Vec::new();
    loop {
        let line = read_line_wait(reader)?;
        if line.is_empty() {
            bail!("connection closed while reading headers");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Ok(HttpResponse { status, headers })
}

fn read_line_wait(reader: &mut impl BufRead) -> Result<String> {
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) if line.is_empty() => return Ok(String::new()),
            Ok(_) => return Ok(line),
            Err(err) if err.kind() == io::ErrorKind::TimedOut || err.kind() == io::ErrorKind::WouldBlock => {
                if cancelled() {
                    bail!("cancelled");
                }
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the server");
                }
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn parse_status(line: &str) -> Result<u16> {
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    let code = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        bail!("bad status line: {}", line.trim());
    }
    code.parse().with_context(|| format!("bad status line: {}", line.trim()))
}

fn drain_body(reader: &mut impl BufRead, headers: &[(String, String)], keep_text: bool) -> Result<String> {
    if let Some((_, value)) = headers.iter().find(|(name, _)| name == "content-length") {
        let len: u64 = value.parse().context("Content-Length")?;
        return read_limited(reader, len, keep_text);
    }
    if headers.iter().any(|(name, value)| name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")) {
        return read_chunked(reader, keep_text);
    }
    Ok(String::new())
}

fn read_limited(reader: &mut impl BufRead, len: u64, keep_text: bool) -> Result<String> {
    let mut left = len;
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..want]).context("truncated response body")?;
        if keep_text && kept.len() < 512 {
            let take = want.min(512 - kept.len());
            kept.extend_from_slice(&buf[..take]);
        }
        left -= want as u64;
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

fn read_chunked(reader: &mut impl BufRead, keep_text: bool) -> Result<String> {
    let mut kept = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size_token = size_line.trim().split(';').next().unwrap_or("0");
        let size = u64::from_str_radix(size_token, 16).context("bad chunk size")?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                if trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        let text = read_limited(reader, size, keep_text && kept.len() < 512)?;
        if kept.len() < 512 {
            let room = 512 - kept.len();
            kept.extend(text.as_bytes().iter().take(room).copied());
        }
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

fn connect(server: &Url) -> Result<BufReader<Box<dyn Io + Send>>> {
    let host = server.host_str().context("server has no host")?;
    let port = server.port_or_known_default().context("server has no port")?;
    let tcp = TcpStream::connect((host, port)).with_context(|| format!("connect {host}:{port}"))?;
    prepare_tcp(&tcp, Duration::from_secs(30));
    finish_connect(server, host, tcp)
}

fn connect_within(server: &Url, timeout: Duration) -> Result<BufReader<Box<dyn Io + Send>>> {
    use std::net::ToSocketAddrs;
    let host = server.host_str().context("server has no host")?;
    let port = server.port_or_known_default().context("server has no port")?;
    let mut last_err = None;
    for addr in (host, port).to_socket_addrs().with_context(|| format!("resolve {host}"))? {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(tcp) => {
                prepare_tcp(&tcp, timeout);
                return finish_connect(server, host, tcp);
            }
            Err(err) => last_err = Some(err),
        }
    }
    match last_err {
        Some(err) => Err(err).with_context(|| format!("connect {host}:{port}")),
        None => bail!("connect {host}:{port}"),
    }
}

fn prepare_tcp(tcp: &TcpStream, timeout: Duration) {
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
}

fn finish_connect(server: &Url, host: &str, tcp: TcpStream) -> Result<BufReader<Box<dyn Io + Send>>> {
    match server.scheme() {
        "https" => {
            let tls = connect_tls(host, tcp)?;
            Ok(BufReader::new(tls))
        }
        "http" => {
            tighten_timeouts(&tcp);
            Ok(BufReader::new(Box::new(tcp)))
        }
        other => bail!("unsupported scheme {other}"),
    }
}

fn tighten_timeouts(tcp: &TcpStream) {
    tcp.set_read_timeout(Some(READ_POLL)).ok();
    tcp.set_write_timeout(Some(IO_TIMEOUT)).ok();
}

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

pub fn check_credentials(server: &Url, user: &str, password: &str) -> Result<()> {
    let reply = http_exchange(server, user, password, "GET", "/", &[("X-Current-Only", "1")], &[], 2 * 1024 * 1024)?;
    match reply.status {
        200 => Ok(()),
        401 | 403 => bail!("That subscription number or password was not accepted."),
        status => bail!("Could not reach the server (HTTP {status})."),
    }
}

pub fn get_bytes(url: &Url, max_body: usize) -> Result<Vec<u8>> {
    let host = host_header(url);
    let target = request_path(url);
    let mut conn = connect_within(url, Duration::from_secs(20))?;
    {
        let sock = conn.get_mut();
        write!(sock, "GET {target} HTTP/1.1\r\n")?;
        write!(sock, "Host: {host}\r\n")?;
        write!(sock, "User-Agent: deppo/0.1\r\n")?;
        write!(sock, "Connection: close\r\n\r\n")?;
        sock.flush()?;
    }
    let response = read_response(&mut conn)?;
    if response.status != 200 {
        bail!("download failed ({})", response.status);
    }
    read_body_bytes(&mut conn, &response.headers, max_body)
}

fn request_path(url: &Url) -> String {
    let mut target = if url.path().is_empty() { "/".to_string() } else { url.path().to_string() };
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    target
}

pub fn post_json(url: &Url, body: &[u8]) -> Result<HttpReply> {
    let host = host_header(url);
    let target = request_path(url);
    let mut conn = connect_within(url, Duration::from_secs(5))?;
    {
        let sock = conn.get_mut();
        write!(sock, "POST {target} HTTP/1.1\r\n")?;
        write!(sock, "Host: {host}\r\n")?;
        write!(sock, "Content-Type: application/json\r\n")?;
        write!(sock, "Content-Length: {}\r\n", body.len())?;
        write!(sock, "User-Agent: deppo/0.1\r\n")?;
        write!(sock, "Connection: close\r\n\r\n")?;
        sock.write_all(body)?;
        sock.flush()?;
    }
    let response = read_response(&mut conn)?;
    let bytes = read_body_bytes(&mut conn, &response.headers, 64 * 1024)?;
    Ok(HttpReply { status: response.status, body: bytes })
}

pub fn http_exchange(server: &Url, user: &str, password: &str, method: &str, target: &str, extra: &[(&str, &str)], body: &[u8], max_body: usize) -> Result<HttpReply> {
    let host = host_header(server);
    let auth = basic_authorization(user, password);
    let mut conn = connect(server)?;
    {
        let sock = conn.get_mut();
        write!(sock, "{method} {target} HTTP/1.1\r\n")?;
        write!(sock, "Host: {host}\r\n")?;
        write!(sock, "Authorization: {auth}\r\n")?;
        for (name, value) in extra {
            write!(sock, "{name}: {value}\r\n")?;
        }
        if !body.is_empty() && !extra.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type")) {
            write!(sock, "Content-Type: application/octet-stream\r\n")?;
        }
        write!(sock, "Content-Length: {}\r\n", body.len())?;
        write!(sock, "User-Agent: deppo/0.1\r\n")?;
        write!(sock, "Connection: close\r\n\r\n")?;
        sock.write_all(body)?;
        sock.flush()?;
    }
    let response = read_response(&mut conn)?;
    let bytes = read_body_bytes(&mut conn, &response.headers, max_body)?;
    Ok(HttpReply { status: response.status, body: bytes })
}

fn read_body_bytes(reader: &mut impl BufRead, headers: &[(String, String)], max_body: usize) -> Result<Vec<u8>> {
    if let Some((_, value)) = headers.iter().find(|(name, _)| name == "content-length") {
        let len: usize = value.parse().context("Content-Length")?;
        if len > max_body {
            bail!("response is too large");
        }
        let mut buf = vec![0u8; len];
        if len > 0 {
            reader.read_exact(&mut buf).context("truncated response body")?;
        }
        return Ok(buf);
    }
    if headers.iter().any(|(name, value)| name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")) {
        return read_chunked_bytes(reader, max_body);
    }
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() + n > max_body {
                    bail!("response is too large");
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::TimedOut || err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the server");
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(buf)
}

fn read_chunked_bytes(reader: &mut impl BufRead, max_body: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    loop {
        let mut size_line = String::new();
        reader.read_line(&mut size_line)?;
        let size_token = size_line.trim().split(';').next().unwrap_or("0");
        let size = usize::from_str_radix(size_token, 16).context("bad chunk size")?;
        if size == 0 {
            loop {
                let mut trailer = String::new();
                reader.read_line(&mut trailer)?;
                if trailer.trim().is_empty() {
                    break;
                }
            }
            break;
        }
        if buf.len() + size > max_body {
            bail!("response is too large");
        }
        let start = buf.len();
        buf.resize(start + size, 0);
        reader.read_exact(&mut buf[start..]).context("truncated chunk")?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
    Ok(buf)
}

fn connect_tls(host: &str, tcp: TcpStream) -> Result<Box<dyn Io + Send>> {
    let connector = native_tls::TlsConnector::new().context("TLS")?;
    let stream = connector.connect(host, tcp).with_context(|| format!("TLS handshake with {host}"))?;
    tighten_timeouts(stream.get_ref());
    Ok(Box::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_paths() {
        assert_eq!(logical_from_display(r"notes\a.txt"), "/notes/a.txt");
        assert_eq!(logical_from_display(r"C:\Users\tan\Desktop\myfile.txt"), "/C/Users/tan/Desktop/myfile.txt");
        assert_eq!(logical_from_display(r"\\?\C:\Users\tan\Desktop\myfile.txt"), "/C/Users/tan/Desktop/myfile.txt");
        assert_eq!(logical_from_display(r"\\server\share\file"), "/server/share/file");
    }

    #[test]
    fn request_path_encodes_spaces() {
        let server = Url::parse("https://h1.deppo.eu").unwrap();
        assert_eq!(request_target(&server, "/C/Users/tan/my file.txt"), "/C/Users/tan/my%20file.txt");
    }

    #[test]
    fn status_parser() {
        assert_eq!(parse_status("HTTP/1.1 412 Precondition Failed\r\n").unwrap(), 412);
        assert_eq!(parse_status("HTTP/1.1 100 Continue\r\n").unwrap(), 100);
    }

    #[test]
    fn authorization_is_standard_base64() {
        assert_eq!(basic_authorization("12345678", "secret"), "Basic MTIzNDU2Nzg6c2VjcmV0");
    }

    #[test]
    fn extra_continue_is_not_a_failure() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n";
        let mut reader = io::Cursor::new(&raw[..]);
        let mut sent = 0;
        let response = read_until_final(&mut reader, |_| {
            sent += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(sent, 1);
        assert_eq!(response.status, 201);
    }

    #[test]
    fn unchanged_response_does_not_send_the_body() {
        let raw = b"HTTP/1.1 412 Precondition Failed\r\nContent-Length: 14\r\n\r\nfile unchanged";
        let mut reader = io::Cursor::new(&raw[..]);
        let mut sent = 0;
        let response = read_until_final(&mut reader, |_| {
            sent += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(sent, 0);
        assert_eq!(response.status, 412);
    }
}
