# Sprint 3 (PLAN Phase 3: storage and settings) + model downloads (PLAN Phase 4, adapter part)

Rules: [RULES.md](./RULES.md). Architecture: [../ARCHITECTURE.md](../ARCHITECTURE.md).

| Ticket | Owner | Status |
|---|---|---|
| T3-1 `adapters/store.rs` (SQLite: history, day totals, retention) | store-dev | todo |
| T3-2 `adapters/settings_file.rs` (load, migrate, atomic save, corrupt recovery) | store-dev | todo |
| T3-3 `settings_apply.rs` + settings IPC (exhaustive apply, hotkey re-register, model preload, autostart) | orchestrator | todo |
| T3-4 spool recovery on start + retry of failed/recovered items | orchestrator | todo |
| T4-1 `adapters/models.rs` (download, resume, verify, cancel, delete, status) | models-dev | todo |

## T3-1 store

`//! SOT: db-schema` header. Add `db-schema` to `REQUIRED_SOT_IDS` in `scripts/guardrails.sh`.

```rust
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)] #[serde(rename_all = "snake_case")]
pub enum TranscriptStatus { Ok, Failed, Recovered }
#[derive(Clone, Serialize, specta::Type)]   // Debug implemented by hand: redact text and raw_text (G7)
pub struct TranscriptRow {
    pub id: String, pub created_at_ms: i64, pub status: TranscriptStatus,
    pub text: Option<String>, pub raw_text: Option<String>, pub language: Option<Language>,
    pub duration_ms: u64, pub latin_words: u32, pub cjk_chars: u32, pub model_id: Option<String>,
    pub latency_ms: Option<u64>, pub asr_ms: Option<u64>, pub audio_path: Option<String>, pub error: Option<String>,
}   // u64/i64 fields need #[specta(type = f64)] (JS numbers)
pub struct Store { /* Mutex<rusqlite::Connection> */ }
impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError>;         // WAL, foreign_keys, busy_timeout 2 s, migrations
    pub fn open_in_memory() -> Result<Store, StoreError>;
    pub fn insert(&self, row: &TranscriptRow) -> Result<(), StoreError>;
    pub fn complete_retry(&self, id: &str, row: &TranscriptRow) -> Result<(), StoreError>; // failed/recovered -> ok, audio_path cleared
    pub fn set_error(&self, id: &str, error: &str) -> Result<(), StoreError>;
    pub fn get(&self, id: &str) -> Result<Option<TranscriptRow>, StoreError>;
    pub fn list(&self, query: &str, limit: u32, offset: u32) -> Result<Vec<TranscriptRow>, StoreError>; // newest first; LIKE with escaped % and _
    pub fn count(&self, query: &str) -> Result<u32, StoreError>;
    pub fn delete(&self, id: &str) -> Result<Option<String>, StoreError>;      // returns audio_path to delete
    pub fn delete_all(&self) -> Result<Vec<String>, StoreError>;
    pub fn purge_before(&self, cutoff_ms: i64) -> Result<Vec<String>, StoreError>;
    pub fn audio_paths(&self) -> Result<std::collections::HashSet<String>, StoreError>;
    pub fn add_day_totals(&self, day: i32, language: Language, counts: Counts, speech_ms: u64) -> Result<(), StoreError>;
    pub fn day_totals(&self) -> Result<Vec<DayTotals>, StoreError>;
    pub fn recent_latencies(&self, n: u32) -> Result<Vec<u64>, StoreError>;
    pub fn reset_stats(&self) -> Result<(), StoreError>;
}
pub fn local_day(ms: i64) -> i32;   // days since 1970-01-01 in the local time zone (chrono)
```

Acceptance:
- Schema = PLAN 2.10 `transcripts` (+ index on created_at) and `daily_stats(day INTEGER, language TEXT,
  sessions, latin_words, cjk_chars, speech_ms, PRIMARY KEY(day, language))` (no saved_ms; see ARCHITECTURE
  deviations). Migrations are an append-only array applied with `PRAGMA user_version`; a test proves an old
  version upgrades and that migration 1 never changes once shipped (hash or snapshot test).
- Deleting history never touches `daily_stats` (totals survive); `reset_stats` clears them.
- Tests on a temp file DB: CRUD, search with `%`/`_`/Japanese, paging, retention purge, day totals upsert,
  latencies order, concurrent readers while writing (WAL), reopening keeps data. 100k rows: `list` with a
  query under 50 ms (ignored perf test).

## T3-2 settings_file

```rust
pub struct Loaded { pub settings: Settings, pub recovered: bool }   // recovered = the old file was corrupt
pub fn load(path: &Path) -> Loaded;                                    // missing -> defaults; corrupt -> moved to settings.json.bak, defaults
pub fn save(path: &Path, settings: &Settings) -> Result<(), SettingsFileError>;  // tmp file + fsync + rename
```
Uses `Settings::migrate` and `Settings::validate` (invalid values -> that field reset to its default, logged).
Tests: missing, corrupt, future version (keep the file untouched, run on defaults, `recovered = true` but do
not overwrite it until the user changes a setting), round trip, atomicity (no partial file on error).

## T4-1 models

The only HTTP user (G11). `reqwest` with `rustls`, streaming, no default features you do not need.

```rust
#[derive(Clone, Serialize, specta::Type)]
pub struct ModelStatus { pub id: String, pub installed: bool, pub downloading: Option<DownloadProgress>, pub error: Option<String> }
#[derive(Clone, Serialize, specta::Type)]
pub struct DownloadProgress { #[specta(type = f64)] pub received: u64, #[specta(type = f64)] pub total: u64, pub bytes_per_sec: f64 }
pub struct ModelStore { /* dir + in-flight downloads */ }
impl ModelStore {
    pub fn new(dir: PathBuf) -> ModelStore;
    pub fn path_if_installed(&self, entry: &ModelEntry) -> Option<PathBuf>;  // size matches + verified marker matches
    pub fn spec(&self, id: &str) -> Option<ModelSpec>;                       // catalog lookup + path_if_installed
    pub fn statuses(&self) -> Vec<ModelStatus>;
    pub async fn download(&self, id: &str, on_progress: impl Fn(DownloadProgress) + Send + Sync + 'static) -> Result<PathBuf, ModelError>;
    pub fn cancel(&self, id: &str);
    pub fn delete(&self, id: &str) -> Result<(), ModelError>;
}
```

Acceptance:
- Writes `<file>.part`; resumes with `Range` from the part size; if the server ignores the range, restarts.
  Stall timeout: 30 s without bytes -> `ModelError::Stalled` (part kept, resumable). Cancel keeps the part.
- SHA-256 over the whole file after download (stream the part file once more if resumed). Mismatch -> delete
  part, `ModelError::Checksum`. Success -> rename to the final name, write `<file>.verified` with
  `{size, sha256, mtime}`; `path_if_installed` trusts the marker only if size and mtime still match.
- Disk space check before download (`statvfs`): refuse with `ModelError::DiskFull` if free < remaining + 500 MB.
- Offline: a clear `ModelError::Offline` (DNS/connect failure) the UI can show.
- Tests: a local test HTTP server on 127.0.0.1 (std `TcpListener`, a few lines, test-only; no new runtime
  dependency) serving a small file with and without Range support, a stall, a checksum mismatch, and resume
  after cancel. The catalog entry is injected in tests (a `download_entry(&ModelEntry, ...)` internal fn).
- One real download of the VAD model (1.2 MB) in an `#[ignore]` test.
