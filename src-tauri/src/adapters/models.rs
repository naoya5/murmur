//! Adapter: model files. Download with resume, verify, cancel, delete, status.
//!
//! This is the only file that talks HTTP (guardrail G11). A download writes `<file>.part`, resumes with a
//! `Range` request from the part size, checks SHA-256 over the whole file, then renames it to `<file>` and
//! writes `<file>.verified` (size, sha256, mtime). `path_if_installed` trusts the marker only while the file
//! still has that size and mtime, so a cheap `stat` replaces hashing 600 MB at every start.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use murmur_core::catalog::{self, ModelEntry};
use murmur_core::ports::ModelSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// No bytes for this long ends the download with `Stalled` (the part is kept).
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Free space that must remain after the download.
const DISK_RESERVE: u64 = 500 * 1024 * 1024;
/// How often the loop looks at the cancel flag while waiting for bytes.
const CANCEL_POLL: Duration = Duration::from_millis(100);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct DownloadProgress {
    #[specta(type = f64)]
    pub received: u64,
    #[specta(type = f64)]
    pub total: u64,
    pub bytes_per_sec: f64,
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct ModelStatus {
    pub id: String,
    pub installed: bool,
    pub downloading: Option<DownloadProgress>,
    /// The error of the last failed download, until the next one starts.
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unknown model {0}")]
    UnknownModel(String),
    #[error("a download of this model is already running")]
    AlreadyDownloading,
    #[error("no network connection")]
    Offline,
    #[error("the download stalled")]
    Stalled,
    #[error("the download was cancelled")]
    Cancelled,
    #[error("the downloaded file is damaged (checksum mismatch)")]
    Checksum,
    #[error("not enough free disk space")]
    DiskFull,
    #[error("the server answered with HTTP {0}")]
    Http(u16),
    #[error("network error: {0}")]
    Network(String),
    #[error("file error: {0}")]
    Io(String),
}

fn io_err(e: std::io::Error) -> ModelError {
    ModelError::Io(e.to_string())
}

fn net_err(e: reqwest::Error) -> ModelError {
    if e.is_connect() {
        ModelError::Offline
    } else if e.is_timeout() {
        ModelError::Stalled
    } else {
        ModelError::Network(e.without_url().to_string())
    }
}

#[derive(Serialize, Deserialize)]
struct Marker {
    size: u64,
    sha256: String,
    mtime_ms: i64,
}

struct Active {
    cancel: Arc<AtomicBool>,
    progress: DownloadProgress,
}

#[derive(Default)]
struct State {
    active: HashMap<String, Active>,
    errors: HashMap<String, String>,
}

/// Removes the in-flight entry on every exit path.
struct ActiveGuard {
    state: Arc<Mutex<State>>,
    id: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        lock(&self.state).active.remove(&self.id);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct ModelStore {
    dir: PathBuf,
    state: Arc<Mutex<State>>,
    stall_timeout: Duration,
    disk_reserve: u64,
}

impl ModelStore {
    pub fn new(dir: PathBuf) -> ModelStore {
        ModelStore {
            dir,
            state: Arc::new(Mutex::new(State::default())),
            stall_timeout: STALL_TIMEOUT,
            disk_reserve: DISK_RESERVE,
        }
    }

    fn final_path(&self, entry: &ModelEntry) -> PathBuf {
        self.dir.join(entry.file)
    }

    fn part_path(&self, entry: &ModelEntry) -> PathBuf {
        self.dir.join(format!("{}.part", entry.file))
    }

    fn marker_path(&self, entry: &ModelEntry) -> PathBuf {
        self.dir.join(format!("{}.verified", entry.file))
    }

    /// The model file if it is installed: size matches and the verified marker matches size, sha256 and mtime.
    pub fn path_if_installed(&self, entry: &ModelEntry) -> Option<PathBuf> {
        let path = self.final_path(entry);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() != entry.bytes {
            return None;
        }
        let marker: Marker =
            serde_json::from_slice(&std::fs::read(self.marker_path(entry)).ok()?).ok()?;
        let same = marker.size == meta.len()
            && marker.sha256.eq_ignore_ascii_case(entry.sha256)
            && Some(marker.mtime_ms) == mtime_ms(&meta);
        same.then_some(path)
    }

    pub fn spec(&self, id: &str) -> Option<ModelSpec> {
        let entry = catalog::find(id)?;
        let path = self.path_if_installed(entry)?;
        Some(ModelSpec { entry, path })
    }

    pub fn statuses(&self) -> Vec<ModelStatus> {
        let state = lock(&self.state);
        catalog::CATALOG
            .iter()
            .map(|entry| ModelStatus {
                id: entry.id.to_owned(),
                installed: self.path_if_installed(entry).is_some(),
                downloading: state.active.get(entry.id).map(|a| a.progress.clone()),
                error: state.errors.get(entry.id).cloned(),
            })
            .collect()
    }

    /// Asks a running download to stop. The part file is kept so the download can resume.
    pub fn cancel(&self, id: &str) {
        if let Some(active) = lock(&self.state).active.get(id) {
            active.cancel.store(true, Ordering::Release);
        }
    }

    /// Removes the model, its part file and its marker. Cancels a running download first.
    pub fn delete(&self, id: &str) -> Result<(), ModelError> {
        let entry = catalog::find(id).ok_or_else(|| ModelError::UnknownModel(id.to_owned()))?;
        self.cancel(id);
        for path in [
            self.final_path(entry),
            self.part_path(entry),
            self.marker_path(entry),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        lock(&self.state).errors.remove(id);
        Ok(())
    }

    pub async fn download(
        &self,
        id: &str,
        on_progress: impl Fn(DownloadProgress) + Send + Sync + 'static,
    ) -> Result<PathBuf, ModelError> {
        let entry = catalog::find(id).ok_or_else(|| ModelError::UnknownModel(id.to_owned()))?;
        self.download_entry(entry, &entry.url(), &on_progress).await
    }

    async fn download_entry(
        &self,
        entry: &ModelEntry,
        url: &str,
        on_progress: &(impl Fn(DownloadProgress) + Send + Sync),
    ) -> Result<PathBuf, ModelError> {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock(&self.state);
            if state.active.contains_key(entry.id) {
                return Err(ModelError::AlreadyDownloading);
            }
            state.errors.remove(entry.id);
            state.active.insert(
                entry.id.to_owned(),
                Active {
                    cancel: Arc::clone(&cancel),
                    progress: DownloadProgress {
                        received: 0,
                        total: entry.bytes,
                        bytes_per_sec: 0.0,
                    },
                },
            );
        }
        let _guard = ActiveGuard {
            state: Arc::clone(&self.state),
            id: entry.id.to_owned(),
        };
        let result = self.run_download(entry, url, &cancel, on_progress).await;
        if let Err(e) = &result
            && !matches!(e, ModelError::Cancelled)
        {
            lock(&self.state)
                .errors
                .insert(entry.id.to_owned(), e.to_string());
        }
        result
    }

    async fn run_download(
        &self,
        entry: &ModelEntry,
        url: &str,
        cancel: &AtomicBool,
        on_progress: &(impl Fn(DownloadProgress) + Send + Sync),
    ) -> Result<PathBuf, ModelError> {
        if let Some(path) = self.path_if_installed(entry) {
            return Ok(path);
        }
        std::fs::create_dir_all(&self.dir).map_err(io_err)?;
        let part = self.part_path(entry);
        let mut offset = match std::fs::metadata(&part) {
            Ok(m) if m.len() <= entry.bytes => m.len(),
            Ok(_) => {
                std::fs::remove_file(&part).map_err(io_err)?;
                0
            }
            Err(_) => 0,
        };

        // Fail early on a full disk instead of halfway through (statvfs failing means no check).
        if let Some(free) = free_bytes(&self.dir)
            && free < (entry.bytes - offset).saturating_add(self.disk_reserve)
        {
            return Err(ModelError::DiskFull);
        }

        if offset < entry.bytes {
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(net_err)?;
            let (response, start) = self.open_stream(&client, url, &part, offset).await?;
            offset = start;
            self.stream_to_part(entry, response, &part, offset, cancel, on_progress)
                .await?;
        }

        // Hash the whole file once (also covers a resumed download).
        let hash_path = part.clone();
        let digest = tokio::task::spawn_blocking(move || sha256_file(&hash_path))
            .await
            .map_err(|e| ModelError::Io(e.to_string()))?
            .map_err(io_err)?;
        if !digest.eq_ignore_ascii_case(entry.sha256) {
            let _ = std::fs::remove_file(&part);
            return Err(ModelError::Checksum);
        }
        let final_path = self.final_path(entry);
        std::fs::rename(&part, &final_path).map_err(io_err)?;
        let meta = std::fs::metadata(&final_path).map_err(io_err)?;
        let marker = Marker {
            size: meta.len(),
            sha256: digest,
            mtime_ms: mtime_ms(&meta).unwrap_or(0),
        };
        self.write_marker(entry, &marker)?;
        Ok(final_path)
    }

    fn write_marker(&self, entry: &ModelEntry, marker: &Marker) -> Result<(), ModelError> {
        let json = serde_json::to_vec(marker).map_err(|e| ModelError::Io(e.to_string()))?;
        let tmp = self.dir.join(format!("{}.verified.tmp", entry.file));
        std::fs::write(&tmp, json).map_err(io_err)?;
        std::fs::rename(&tmp, self.marker_path(entry)).map_err(io_err)
    }

    /// Sends the (ranged) request. Returns the response and the offset the body starts at.
    /// A server that ignores `Range` answers 200: the part is then restarted from 0.
    async fn open_stream(
        &self,
        client: &reqwest::Client,
        url: &str,
        part: &Path,
        mut offset: u64,
    ) -> Result<(reqwest::Response, u64), ModelError> {
        // At most one restart from 0 after a rejected range.
        for _ in 0..2 {
            let mut request = client.get(url);
            if offset > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            let response = tokio::time::timeout(self.stall_timeout, request.send())
                .await
                .map_err(|_| ModelError::Stalled)?
                .map_err(net_err)?;
            let status = response.status().as_u16();
            match status {
                206 => {
                    let expected = format!("bytes {offset}-");
                    let ok = response
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.starts_with(&expected));
                    if ok {
                        return Ok((response, offset));
                    }
                }
                200 => return Ok((response, 0)),
                416 if offset > 0 => {}
                other => return Err(ModelError::Http(other)),
            }
            // Range not usable: forget the part and ask for the whole file.
            let _ = std::fs::remove_file(part);
            offset = 0;
        }
        Err(ModelError::Http(416))
    }

    async fn stream_to_part(
        &self,
        entry: &ModelEntry,
        mut response: reqwest::Response,
        part: &Path,
        start: u64,
        cancel: &AtomicBool,
        on_progress: &(impl Fn(DownloadProgress) + Send + Sync),
    ) -> Result<(), ModelError> {
        let mut file = if start == 0 {
            File::create(part)
        } else {
            OpenOptions::new().append(true).open(part)
        }
        .map_err(io_err)?;
        let mut received = start;
        let began = Instant::now();
        let mut last_data = Instant::now();
        let mut last_report = Instant::now() - PROGRESS_INTERVAL;
        loop {
            if cancel.load(Ordering::Acquire) {
                file.flush().map_err(io_err)?;
                return Err(ModelError::Cancelled);
            }
            match tokio::time::timeout(CANCEL_POLL, response.chunk()).await {
                Err(_) => {
                    if last_data.elapsed() >= self.stall_timeout {
                        file.flush().map_err(io_err)?;
                        return Err(ModelError::Stalled);
                    }
                }
                Ok(Err(e)) => {
                    file.flush().map_err(io_err)?;
                    return Err(net_err(e));
                }
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => {
                    last_data = Instant::now();
                    file.write_all(&chunk).map_err(io_err)?;
                    received += chunk.len() as u64;
                    if last_report.elapsed() >= PROGRESS_INTERVAL {
                        last_report = Instant::now();
                        self.report(entry, received, start, began, on_progress);
                    }
                }
            }
        }
        file.sync_all().map_err(io_err)?;
        self.report(entry, received, start, began, on_progress);
        if received < entry.bytes {
            // The server closed early. The part is kept so the next try resumes.
            return Err(ModelError::Stalled);
        }
        Ok(())
    }

    fn report(
        &self,
        entry: &ModelEntry,
        received: u64,
        start: u64,
        began: Instant,
        on_progress: &(impl Fn(DownloadProgress) + Send + Sync),
    ) {
        let secs = began.elapsed().as_secs_f64().max(0.001);
        let progress = DownloadProgress {
            received,
            total: entry.bytes,
            bytes_per_sec: (received - start) as f64 / secs,
        };
        if let Some(active) = lock(&self.state).active.get_mut(entry.id) {
            active.progress = progress.clone();
        }
        on_progress(progress);
    }
}

fn mtime_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Free bytes available to this user on the volume holding `dir`; `None` when it cannot be read.
fn free_bytes(dir: &Path) -> Option<u64> {
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stat` points to writable memory of the right type.
    let rc = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled the struct.
    let stat = unsafe { stat.assume_init() };
    Some(u64::from(stat.f_bavail).saturating_mul(stat.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        /// Serves everything, honours Range.
        Normal,
        /// Honours Range, but sends the body slowly so a test can cancel in the middle.
        Slow,
        /// Always answers 200 with the full body.
        IgnoreRange,
        /// Sends this many bytes of the body, then goes silent.
        StallAfter(usize),
        /// Answers 503.
        Fail,
    }

    struct TestServer {
        url: String,
        /// The Range header of each request (empty string = none).
        ranges: Arc<Mutex<Vec<String>>>,
        hits: Arc<AtomicUsize>,
    }

    fn body_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn sha_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn serve(body: Vec<u8>, mode: Mode) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (r, h) = (Arc::clone(&ranges), Arc::clone(&hits));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (body, r, h) = (body.clone(), Arc::clone(&r), Arc::clone(&h));
                std::thread::spawn(move || {
                    let _ = handle(stream, &body, mode, &r, &h);
                });
            }
        });
        TestServer { url, ranges, hits }
    }

    fn handle(
        mut stream: TcpStream,
        body: &[u8],
        mode: Mode,
        ranges: &Mutex<Vec<String>>,
        hits: &AtomicUsize,
    ) -> std::io::Result<()> {
        let mut reader = std::io::BufReader::new(stream.try_clone()?);
        let mut range = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
                range = v.trim().to_owned();
            }
        }
        hits.fetch_add(1, Ordering::SeqCst);
        ranges.lock().unwrap().push(range.clone());
        if mode == Mode::Fail {
            return stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
        let start = if mode == Mode::IgnoreRange {
            0
        } else {
            range
                .strip_prefix("bytes=")
                .and_then(|r| r.strip_suffix('-'))
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
        };
        let part = &body[start..];
        if start > 0 {
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len() - 1,
                body.len(),
                part.len()
            )?;
        } else {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                part.len()
            )?;
        }
        match mode {
            Mode::StallAfter(n) => {
                stream.write_all(&part[..n.min(part.len())])?;
                stream.flush()?;
                std::thread::sleep(Duration::from_secs(30));
            }
            Mode::Slow => {
                for piece in part.chunks(8 * 1024) {
                    stream.write_all(piece)?;
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            _ => stream.write_all(part)?,
        }
        Ok(())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("murmur-models-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A catalog entry for `body`, with a unique id so parallel tests do not share state.
    fn entry_for(body: &[u8], id: &'static str) -> ModelEntry {
        ModelEntry {
            id,
            file: "model.bin",
            bytes: body.len() as u64,
            sha256: Box::leak(sha_hex(body).into_boxed_str()),
            ..*catalog::vad()
        }
    }

    fn store(dir: &Path) -> ModelStore {
        let mut store = ModelStore::new(dir.to_path_buf());
        store.disk_reserve = 1024; // the real 500 MB reserve is checked in its own test
        store
    }

    #[tokio::test]
    async fn downloads_verifies_and_marks_installed() {
        let body = body_bytes(300_000);
        let server = serve(body.clone(), Mode::Normal);
        let dir = temp_dir("ok");
        let store = store(&dir);
        let entry = entry_for(&body, "t-ok");
        assert!(store.path_if_installed(&entry).is_none());

        let seen = Arc::new(Mutex::new(Vec::<DownloadProgress>::new()));
        let sink = Arc::clone(&seen);
        let path = store
            .download_entry(&entry, &server.url, &move |p| sink.lock().unwrap().push(p))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body);
        assert_eq!(store.path_if_installed(&entry), Some(path.clone()));
        assert!(!store.part_path(&entry).exists());
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!((last.received, last.total), (300_000, 300_000));
        // A second call is a no-op: no request is made.
        let hits = server.hits.load(Ordering::SeqCst);
        store
            .download_entry(&entry, &server.url, &|_| {})
            .await
            .unwrap();
        assert_eq!(server.hits.load(Ordering::SeqCst), hits);

        // A file that changed after verification is no longer trusted.
        let mut changed = body.clone();
        changed[10] ^= 0xff;
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&path, changed).unwrap();
        assert!(store.path_if_installed(&entry).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cancel_keeps_part_and_resume_uses_range() {
        let body = body_bytes(1_000_000);
        let server = serve(body.clone(), Mode::Slow);
        let dir = temp_dir("resume");
        let store = store(&dir);
        let entry = entry_for(&body, "t-resume");

        let canceller = store.clone();
        let first = store
            .download_entry(&entry, &server.url, &move |p| {
                if p.received > 200_000 {
                    canceller.cancel("t-resume");
                }
            })
            .await;
        assert!(matches!(first, Err(ModelError::Cancelled)), "{first:?}");
        let part_len = std::fs::metadata(store.part_path(&entry)).unwrap().len();
        assert!(part_len > 0 && part_len < body.len() as u64);
        assert!(store.statuses().iter().all(|s| s.downloading.is_none()));

        let path = store
            .download_entry(&entry, &server.url, &|_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), body);
        let ranges = server.ranges.lock().unwrap().clone();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], "");
        assert_eq!(ranges[1], format!("bytes={part_len}-"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn server_without_range_restarts_the_file() {
        let body = body_bytes(100_000);
        let server = serve(body.clone(), Mode::IgnoreRange);
        let dir = temp_dir("norange");
        let store = store(&dir);
        let entry = entry_for(&body, "t-norange");
        std::fs::write(store.part_path(&entry), &body[..40_000]).unwrap();

        let path = store
            .download_entry(&entry, &server.url, &|_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), body);
        assert_eq!(server.ranges.lock().unwrap().clone(), ["bytes=40000-"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn checksum_mismatch_deletes_the_part() {
        let body = body_bytes(50_000);
        let server = serve(body.clone(), Mode::Normal);
        let dir = temp_dir("badsum");
        let store = store(&dir);
        let mut entry = entry_for(&body, "t-badsum");
        entry.sha256 = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = store.download_entry(&entry, &server.url, &|_| {}).await;
        assert!(matches!(result, Err(ModelError::Checksum)), "{result:?}");
        assert!(!store.part_path(&entry).exists());
        assert!(!store.final_path(&entry).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stall_keeps_the_part_and_reports_stalled() {
        let body = body_bytes(100_000);
        let server = serve(body.clone(), Mode::StallAfter(1000));
        let dir = temp_dir("stall");
        let mut store = store(&dir);
        store.stall_timeout = Duration::from_millis(400);
        let entry = entry_for(&body, "t-stall");

        let started = Instant::now();
        let result = store.download_entry(&entry, &server.url, &|_| {}).await;
        assert!(matches!(result, Err(ModelError::Stalled)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            std::fs::metadata(store.part_path(&entry)).unwrap().len(),
            1000
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn offline_http_error_and_disk_full() {
        let body = body_bytes(1000);
        let dir = temp_dir("errors");
        let store = store(&dir);
        let entry = entry_for(&body, "t-errors");

        // Nothing listens on this port: a connection failure is reported as Offline.
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/model.bin", l.local_addr().unwrap())
        };
        let result = store.download_entry(&entry, &closed, &|_| {}).await;
        assert!(matches!(result, Err(ModelError::Offline)), "{result:?}");

        let server = serve(body.clone(), Mode::Fail);
        let result = store.download_entry(&entry, &server.url, &|_| {}).await;
        assert!(matches!(result, Err(ModelError::Http(503))), "{result:?}");

        let mut full = store.clone();
        full.disk_reserve = u64::MAX / 2;
        let result = full.download_entry(&entry, &server.url, &|_| {}).await;
        assert!(matches!(result, Err(ModelError::DiskFull)), "{result:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn statuses_spec_and_delete() {
        let dir = temp_dir("status");
        let store = store(&dir);
        let statuses = store.statuses();
        assert_eq!(statuses.len(), catalog::CATALOG.len());
        assert!(
            statuses
                .iter()
                .all(|s| !s.installed && s.downloading.is_none() && s.error.is_none())
        );
        assert!(store.spec(catalog::VAD_MODEL).is_none());
        assert!(matches!(
            store.delete("nope"),
            Err(ModelError::UnknownModel(_))
        ));
        // Delete removes leftovers even when nothing is installed.
        let vad = catalog::vad();
        std::fs::write(store.part_path(vad), b"x").unwrap();
        store.delete(vad.id).unwrap();
        assert!(!store.part_path(vad).exists());
        assert!(free_bytes(&dir).is_some_and(|b| b > 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    #[ignore = "downloads the 1.2 MB VAD model from Hugging Face"]
    async fn real_vad_download() {
        let dir = temp_dir("real");
        let store = ModelStore::new(dir.clone());
        let path = store.download(catalog::VAD_MODEL, |_| {}).await.unwrap();
        println!("downloaded to {}", path.display());
        assert!(store.spec(catalog::VAD_MODEL).is_some());
        assert!(store.statuses().iter().any(|s| s.installed));
        let _ = std::fs::remove_dir_all(dir);
    }
}
