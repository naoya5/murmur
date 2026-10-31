//! SOT: db-schema
//! What: the SQLite schema (`transcripts`, `daily_stats`), the append-only migration list, and the
//!       `Store` that reads and writes them.
//! Why: history may be deleted, but the lifetime totals must survive, so `daily_stats` is a separate
//!      table that history deletion never touches. Migrations are an append-only array keyed by
//!      `PRAGMA user_version`; a shipped migration never changes (a test pins migration 1).
//! Where: the file lives at `<app data dir>/murmur.db`; the controller calls `Store` after the paste (G9);
//!        IPC exposes history through `src-tauri/src/ipc.rs`.
#![cfg_attr(not(test), allow(dead_code))] // Wired by T3-3 / T2-8 (controller).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use murmur_core::Language;
use murmur_core::metrics::{Counts, DayTotals};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

/// Append-only. Index N holds the SQL that takes `user_version` N to N + 1. Never edit a shipped entry.
const MIGRATIONS: &[&str] = &[
    // Migration 1: the initial schema (PLAN 2.10, with daily_stats keyed by day number and language).
    "CREATE TABLE transcripts (
        id TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL,
        status TEXT NOT NULL,
        text TEXT,
        raw_text TEXT,
        language TEXT,
        duration_ms INTEGER NOT NULL,
        latin_words INTEGER NOT NULL DEFAULT 0,
        cjk_chars INTEGER NOT NULL DEFAULT 0,
        model_id TEXT,
        latency_ms INTEGER,
        asr_ms INTEGER,
        audio_path TEXT,
        error TEXT
    );
    CREATE INDEX idx_transcripts_created ON transcripts(created_at);
    CREATE TABLE daily_stats (
        day INTEGER NOT NULL,
        language TEXT NOT NULL,
        sessions INTEGER NOT NULL DEFAULT 0,
        latin_words INTEGER NOT NULL DEFAULT 0,
        cjk_chars INTEGER NOT NULL DEFAULT 0,
        speech_ms INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, language)
    );",
];

const COLUMNS: &str = "id, created_at, status, text, raw_text, language, duration_ms, latin_words, \
                       cjk_chars, model_id, latency_ms, asr_ms, audio_path, error";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStatus {
    Ok,
    Failed,
    Recovered,
}

impl TranscriptStatus {
    fn as_str(self) -> &'static str {
        match self {
            TranscriptStatus::Ok => "ok",
            TranscriptStatus::Failed => "failed",
            TranscriptStatus::Recovered => "recovered",
        }
    }

    fn parse(s: &str) -> Option<TranscriptStatus> {
        match s {
            "ok" => Some(TranscriptStatus::Ok),
            "failed" => Some(TranscriptStatus::Failed),
            "recovered" => Some(TranscriptStatus::Recovered),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, specta::Type)]
pub struct TranscriptRow {
    pub id: String,
    #[specta(type = f64)]
    pub created_at_ms: i64,
    pub status: TranscriptStatus,
    pub text: Option<String>,
    pub raw_text: Option<String>,
    pub language: Option<Language>,
    #[specta(type = f64)]
    pub duration_ms: u64,
    pub latin_words: u32,
    pub cjk_chars: u32,
    pub model_id: Option<String>,
    #[specta(type = Option<f64>)]
    pub latency_ms: Option<u64>,
    #[specta(type = Option<f64>)]
    pub asr_ms: Option<u64>,
    pub audio_path: Option<String>,
    pub error: Option<String>,
}

/// G7: never print transcript text, not even through `{:?}`.
impl std::fmt::Debug for TranscriptRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact =
            |t: &Option<String>| t.as_ref().map(|s| format!("<{} bytes redacted>", s.len()));
        f.debug_struct("TranscriptRow")
            .field("id", &self.id)
            .field("created_at_ms", &self.created_at_ms)
            .field("status", &self.status)
            .field("text", &redact(&self.text))
            .field("raw_text", &redact(&self.raw_text))
            .field("language", &self.language)
            .field("duration_ms", &self.duration_ms)
            .field("latin_words", &self.latin_words)
            .field("cjk_chars", &self.cjk_chars)
            .field("model_id", &self.model_id)
            .field("latency_ms", &self.latency_ms)
            .field("asr_ms", &self.asr_ms)
            .field("audio_path", &self.audio_path)
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(String),
    #[error("file error: {0}")]
    Io(String),
    #[error("no such history item")]
    NotFound,
    #[error("database schema version {0} is newer than this app supports")]
    FutureSchema(i64),
    #[error("unreadable database value in column {0}")]
    BadValue(&'static str),
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Db(e.to_string())
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// WAL, foreign keys, busy timeout 2 s, migrations.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        Self::init(Connection::open(path)?, MIGRATIONS)
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Self::init(Connection::open_in_memory()?, MIGRATIONS)
    }

    fn init(mut conn: Connection, migrations: &[&str]) -> Result<Store, StoreError> {
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn, migrations)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock cannot leave SQLite in a bad state (it is transactional).
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, row: &TranscriptRow) -> Result<(), StoreError> {
        self.conn().execute(
            &format!("INSERT INTO transcripts ({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"),
            params![
                row.id,
                row.created_at_ms,
                row.status.as_str(),
                row.text,
                row.raw_text,
                row.language.map(Language::code),
                to_i64(row.duration_ms),
                row.latin_words,
                row.cjk_chars,
                row.model_id,
                row.latency_ms.map(to_i64),
                row.asr_ms.map(to_i64),
                row.audio_path,
                row.error,
            ],
        )?;
        Ok(())
    }

    /// Turns a failed or recovered item into an ok one. Keeps `created_at`; clears `audio_path` and `error`.
    pub fn complete_retry(&self, id: &str, row: &TranscriptRow) -> Result<(), StoreError> {
        let changed = self.conn().execute(
            "UPDATE transcripts SET status = 'ok', text = ?2, raw_text = ?3, language = ?4, duration_ms = ?5,
                 latin_words = ?6, cjk_chars = ?7, model_id = ?8, latency_ms = ?9, asr_ms = ?10,
                 audio_path = NULL, error = NULL
             WHERE id = ?1 AND status IN ('failed', 'recovered')",
            params![
                id,
                row.text,
                row.raw_text,
                row.language.map(Language::code),
                to_i64(row.duration_ms),
                row.latin_words,
                row.cjk_chars,
                row.model_id,
                row.latency_ms.map(to_i64),
                row.asr_ms.map(to_i64),
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn set_error(&self, id: &str, error: &str) -> Result<(), StoreError> {
        let changed = self.conn().execute(
            "UPDATE transcripts SET error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<TranscriptRow>, StoreError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached(&format!("SELECT {COLUMNS} FROM transcripts WHERE id = ?1"))?;
        let row = stmt.query_row([id], read_row).optional()?;
        row.transpose()
    }

    /// Newest first. `query` is a plain substring (empty = everything); `%` and `_` are literal.
    pub fn list(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<TranscriptRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM transcripts
             WHERE (?1 IS NULL OR text LIKE ?1 ESCAPE '\\')
             ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt.query_map(params![like_pattern(query), limit, offset], read_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    pub fn count(&self, query: &str) -> Result<u32, StoreError> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM transcripts WHERE (?1 IS NULL OR text LIKE ?1 ESCAPE '\\')",
            [like_pattern(query)],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Returns the audio path to delete (if the item had one). Totals are untouched.
    pub fn delete(&self, id: &str) -> Result<Option<String>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let path: Option<String> = tx
            .query_row(
                "SELECT audio_path FROM transcripts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        tx.execute("DELETE FROM transcripts WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(path)
    }

    /// Deletes all history; returns the audio paths to delete. Totals are untouched.
    pub fn delete_all(&self) -> Result<Vec<String>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let paths = audio_paths_where(&tx, "1 = 1", [])?;
        tx.execute("DELETE FROM transcripts", [])?;
        tx.commit()?;
        Ok(paths)
    }

    /// Retention: deletes `ok` items created before `cutoff_ms`; returns their audio paths (normally none).
    /// Failed and recovered items keep their audio until the user retries or deletes them.
    pub fn purge_before(&self, cutoff_ms: i64) -> Result<Vec<String>, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let paths = audio_paths_where(&tx, "status = 'ok' AND created_at < ?1", [cutoff_ms])?;
        tx.execute(
            "DELETE FROM transcripts WHERE status = 'ok' AND created_at < ?1",
            [cutoff_ms],
        )?;
        tx.commit()?;
        Ok(paths)
    }

    /// Every audio path the history refers to (for spool recovery and orphan cleanup).
    pub fn audio_paths(&self) -> Result<HashSet<String>, StoreError> {
        let conn = self.conn();
        Ok(audio_paths_where(&conn, "1 = 1", [])?.into_iter().collect())
    }

    /// Adds one session to the totals of `day` and `language`.
    pub fn add_day_totals(
        &self,
        day: i32,
        language: Language,
        counts: Counts,
        speech_ms: u64,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO daily_stats (day, language, sessions, latin_words, cjk_chars, speech_ms)
             VALUES (?1, ?2, 1, ?3, ?4, ?5)
             ON CONFLICT(day, language) DO UPDATE SET
                 sessions = sessions + 1,
                 latin_words = latin_words + excluded.latin_words,
                 cjk_chars = cjk_chars + excluded.cjk_chars,
                 speech_ms = speech_ms + excluded.speech_ms",
            params![
                day,
                language.code(),
                counts.latin_words,
                counts.cjk_chars,
                to_i64(speech_ms)
            ],
        )?;
        Ok(())
    }

    pub fn day_totals(&self) -> Result<Vec<DayTotals>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT day, language, sessions, latin_words, cjk_chars, speech_ms FROM daily_stats
             ORDER BY day, language",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i32>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (day, language, sessions, latin_words, cjk_chars, speech_ms) = row?;
            out.push(DayTotals {
                day,
                language: parse_language(&language)
                    .ok_or(StoreError::BadValue("daily_stats.language"))?,
                sessions,
                latin_words,
                cjk_chars,
                speech_ms: to_u64(speech_ms),
            });
        }
        Ok(out)
    }

    /// Latencies (stop -> clipboard) of the newest `n` ok items, newest first.
    pub fn recent_latencies(&self, n: u32) -> Result<Vec<u64>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT latency_ms FROM transcripts WHERE status = 'ok' AND latency_ms IS NOT NULL
             ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([n], |r| r.get::<_, i64>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(to_u64(row?));
        }
        Ok(out)
    }

    pub fn reset_stats(&self) -> Result<(), StoreError> {
        self.conn().execute("DELETE FROM daily_stats", [])?;
        Ok(())
    }
}

/// Days since 1970-01-01 in the local time zone.
pub fn local_day(ms: i64) -> i32 {
    use chrono::{Datelike, TimeZone};
    const DAYS_FROM_CE_TO_EPOCH: i32 = 719_163;
    match chrono::Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.date_naive().num_days_from_ce() - DAYS_FROM_CE_TO_EPOCH,
        // A nonexistent or out-of-range local time: fall back to UTC days.
        None => i32::try_from(ms.div_euclid(86_400_000)).unwrap_or(0),
    }
}

/// Applies the migrations that `PRAGMA user_version` says are missing, each in its own transaction.
fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let applied = usize::try_from(version).map_err(|_| StoreError::FutureSchema(version))?;
    if applied > migrations.len() {
        return Err(StoreError::FutureSchema(version));
    }
    for (index, sql) in migrations.iter().enumerate().skip(applied) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(
            None,
            "user_version",
            i64::try_from(index + 1).unwrap_or(i64::MAX),
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// `%x%` with `\`, `%` and `_` escaped; `None` for an empty query (no filter).
fn like_pattern(query: &str) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    Some(out)
}

fn audio_paths_where<P: rusqlite::Params>(
    conn: &Connection,
    condition: &str,
    params: P,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT audio_path FROM transcripts WHERE audio_path IS NOT NULL AND {condition}"
    ))?;
    let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn parse_language(code: &str) -> Option<Language> {
    Language::ALL.into_iter().find(|l| l.code() == code)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn read_row(r: &Row<'_>) -> rusqlite::Result<Result<TranscriptRow, StoreError>> {
    let status: String = r.get(2)?;
    let language: Option<String> = r.get(5)?;
    let Some(status) = TranscriptStatus::parse(&status) else {
        return Ok(Err(StoreError::BadValue("transcripts.status")));
    };
    let language = match language.as_deref() {
        None => None,
        Some(code) => match parse_language(code) {
            Some(l) => Some(l),
            None => return Ok(Err(StoreError::BadValue("transcripts.language"))),
        },
    };
    Ok(Ok(TranscriptRow {
        id: r.get(0)?,
        created_at_ms: r.get(1)?,
        status,
        text: r.get(3)?,
        raw_text: r.get(4)?,
        language,
        duration_ms: to_u64(r.get(6)?),
        latin_words: r.get(7)?,
        cjk_chars: r.get(8)?,
        model_id: r.get(9)?,
        latency_ms: r.get::<_, Option<i64>>(10)?.map(to_u64),
        asr_ms: r.get::<_, Option<i64>>(11)?.map(to_u64),
        audio_path: r.get(12)?,
        error: r.get(13)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    fn temp_db(name: &str) -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("murmur-store-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("murmur.db"))
    }

    fn row(id: &str, created: i64, text: &str) -> TranscriptRow {
        TranscriptRow {
            id: id.into(),
            created_at_ms: created,
            status: TranscriptStatus::Ok,
            text: Some(text.into()),
            raw_text: Some(text.to_lowercase()),
            language: Some(Language::En),
            duration_ms: 2000,
            latin_words: 3,
            cjk_chars: 0,
            model_id: Some("m".into()),
            latency_ms: Some(150),
            asr_ms: Some(90),
            audio_path: None,
            error: None,
        }
    }

    fn failed(id: &str, created: i64, audio: &str) -> TranscriptRow {
        TranscriptRow {
            status: TranscriptStatus::Failed,
            text: None,
            raw_text: None,
            latency_ms: None,
            asr_ms: None,
            audio_path: Some(audio.into()),
            error: Some("boom".into()),
            ..row(id, created, "")
        }
    }

    #[test]
    fn crud_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let r = row("a", 1000, "Hello world");
        store.insert(&r).unwrap();
        assert_eq!(store.get("a").unwrap(), Some(r));
        assert_eq!(store.get("zzz").unwrap(), None);
        assert!(store.insert(&row("a", 2, "dup")).is_err());
        store.set_error("a", "later").unwrap();
        assert_eq!(
            store.get("a").unwrap().unwrap().error.as_deref(),
            Some("later")
        );
        assert!(matches!(
            store.set_error("nope", "x"),
            Err(StoreError::NotFound)
        ));
        assert_eq!(store.delete("a").unwrap(), None);
        assert_eq!(store.get("a").unwrap(), None);
    }

    #[test]
    fn debug_redacts_text() {
        let shown = format!("{:?}", row("a", 1, "secret sentence"));
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn search_paging_and_escaping() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&row("1", 100, "100% sure")).unwrap();
        store.insert(&row("2", 200, "snake_case name")).unwrap();
        store.insert(&row("3", 300, "plain text")).unwrap();
        store.insert(&row("4", 400, "今日はいい天気です")).unwrap();
        store.insert(&row("5", 500, "back\\slash")).unwrap();
        let ids = |q: &str| -> Vec<String> {
            store
                .list(q, 50, 0)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(ids(""), ["5", "4", "3", "2", "1"], "newest first");
        assert_eq!(ids("%"), ["1"], "% is literal");
        assert_eq!(ids("_"), ["2"], "_ is literal");
        assert_eq!(ids("\\"), ["5"], "backslash is literal");
        assert_eq!(ids("天気"), ["4"]);
        assert_eq!(ids("PLAIN"), ["3"], "ASCII is case-insensitive");
        assert_eq!(ids("nothing"), Vec::<String>::new());
        assert_eq!(store.count("").unwrap(), 5);
        assert_eq!(store.count("天気").unwrap(), 1);
        let page: Vec<String> = store
            .list("", 2, 2)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(page, ["3", "2"]);
        assert!(store.list("", 2, 10).unwrap().is_empty());
    }

    #[test]
    fn retry_and_audio_paths() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&failed("f", 10, "/spool/f.pcm")).unwrap();
        let mut rec = failed("r", 20, "/spool/r.pcm");
        rec.status = TranscriptStatus::Recovered;
        store.insert(&rec).unwrap();
        store.insert(&row("ok", 30, "fine")).unwrap();
        let paths = store.audio_paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("/spool/f.pcm") && paths.contains("/spool/r.pcm"));

        store
            .complete_retry("f", &row("ignored", 999, "Retried text"))
            .unwrap();
        let back = store.get("f").unwrap().unwrap();
        assert_eq!(back.status, TranscriptStatus::Ok);
        assert_eq!(back.text.as_deref(), Some("Retried text"));
        assert_eq!(back.created_at_ms, 10, "creation time is kept");
        assert_eq!((back.audio_path, back.error), (None, None));
        // An ok item cannot be retried again.
        assert!(matches!(
            store.complete_retry("ok", &row("x", 1, "y")),
            Err(StoreError::NotFound)
        ));
        assert_eq!(store.audio_paths().unwrap().len(), 1);
        assert_eq!(store.delete("r").unwrap().as_deref(), Some("/spool/r.pcm"));
    }

    #[test]
    fn retention_purge_and_delete_all_keep_totals() {
        let store = Store::open_in_memory().unwrap();
        store.insert(&row("old", 100, "old")).unwrap();
        store.insert(&failed("oldf", 150, "/spool/o.pcm")).unwrap();
        store.insert(&row("new", 900, "new")).unwrap();
        let counts = Counts {
            latin_words: 3,
            cjk_chars: 0,
        };
        store
            .add_day_totals(19_000, Language::En, counts, 2000)
            .unwrap();

        // Only ok rows are purged: the old failed item keeps its audio.
        assert!(store.purge_before(500).unwrap().is_empty());
        assert_eq!(store.count("").unwrap(), 2);
        assert!(store.get("old").unwrap().is_none());
        assert!(store.get("oldf").unwrap().is_some());
        assert!(store.audio_paths().unwrap().contains("/spool/o.pcm"));
        assert!(store.purge_before(500).unwrap().is_empty());
        assert_eq!(
            store.delete("oldf").unwrap().as_deref(),
            Some("/spool/o.pcm")
        );
        store.insert(&failed("f2", 950, "/spool/2.pcm")).unwrap();
        assert_eq!(store.delete_all().unwrap(), ["/spool/2.pcm"]);
        assert_eq!(store.count("").unwrap(), 0);
        assert_eq!(
            store.day_totals().unwrap().len(),
            1,
            "history deletion keeps totals"
        );
        store.reset_stats().unwrap();
        assert!(store.day_totals().unwrap().is_empty());
    }

    #[test]
    fn day_totals_upsert() {
        let store = Store::open_in_memory().unwrap();
        let en = Counts {
            latin_words: 4,
            cjk_chars: 0,
        };
        let ja = Counts {
            latin_words: 0,
            cjk_chars: 10,
        };
        store
            .add_day_totals(20_000, Language::En, en, 1000)
            .unwrap();
        store.add_day_totals(20_000, Language::En, en, 500).unwrap();
        store.add_day_totals(20_000, Language::Ja, ja, 700).unwrap();
        store.add_day_totals(19_999, Language::Ja, ja, 300).unwrap();
        let totals = store.day_totals().unwrap();
        assert_eq!(totals.len(), 3);
        assert_eq!((totals[0].day, totals[0].language), (19_999, Language::Ja));
        let en_row = &totals[1];
        assert_eq!((en_row.day, en_row.language), (20_000, Language::En));
        assert_eq!(
            (
                en_row.sessions,
                en_row.latin_words,
                en_row.cjk_chars,
                en_row.speech_ms
            ),
            (2, 8, 0, 1500)
        );
        assert_eq!(totals[2].cjk_chars, 10);
    }

    #[test]
    fn latencies_newest_first_ok_only() {
        let store = Store::open_in_memory().unwrap();
        for (i, ms) in [100_u64, 200, 300, 400].into_iter().enumerate() {
            let mut r = row(&format!("r{i}"), i as i64 * 10, "x");
            r.latency_ms = Some(ms);
            store.insert(&r).unwrap();
        }
        store.insert(&failed("f", 100, "/p")).unwrap();
        assert_eq!(store.recent_latencies(3).unwrap(), [400, 300, 200]);
        assert_eq!(store.recent_latencies(10).unwrap().len(), 4);
    }

    #[test]
    fn reopen_keeps_data_and_wal_is_on() {
        let (dir, path) = temp_db("reopen");
        {
            let store = Store::open(&path).unwrap();
            store.insert(&row("a", 1, "kept")).unwrap();
            store
                .add_day_totals(1, Language::En, Counts::default(), 5)
                .unwrap();
            let mode: String = store
                .conn()
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.count("kept").unwrap(), 1);
        assert_eq!(store.day_totals().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn readers_proceed_while_writing() {
        let (dir, path) = temp_db("wal");
        let writer = Store::open(&path).unwrap();
        for i in 0..50 {
            writer.insert(&row(&format!("seed{i}"), i, "seed")).unwrap();
        }
        let reader_path = path.clone();
        let reader = std::thread::spawn(move || {
            let store = Store::open(&reader_path).unwrap();
            let mut last = 0;
            for _ in 0..200 {
                last = store.count("").unwrap();
                assert!(last >= 50);
            }
            last
        });
        for i in 0..200 {
            writer
                .insert(&row(&format!("w{i}"), 1000 + i, "write"))
                .unwrap();
        }
        let seen = reader.join().unwrap();
        assert!(seen <= 250);
        assert_eq!(writer.count("").unwrap(), 250);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn migration_one_is_frozen() {
        // Shipped migrations never change: any edit to migration 1 changes this checksum (FNV-1a).
        let hash = MIGRATIONS[0]
            .bytes()
            .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
        assert_eq!(
            hash, MIGRATION_1_FNV,
            "migration 1 changed after shipping; add a new migration instead"
        );
    }
    const MIGRATION_1_FNV: u64 = 13_705_755_494_882_677_023;

    #[test]
    fn old_database_upgrades() {
        let (dir, path) = temp_db("upgrade");
        {
            // An "old" app version that only knew migration 1.
            let mut conn = Connection::open(&path).unwrap();
            migrate(&mut conn, &MIGRATIONS[..1]).unwrap();
            conn.execute(
                "INSERT INTO transcripts (id, created_at, status, duration_ms) VALUES ('old', 5, 'ok', 100)",
                [],
            )
            .unwrap();
        }
        // The "new" app has a migration 2.
        let newer = [
            MIGRATIONS[0],
            "ALTER TABLE transcripts ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;",
        ];
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn, &newer).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let starred: i64 = conn
            .query_row(
                "SELECT starred FROM transcripts WHERE id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(starred, 0, "old data survives the upgrade");
        // Running again is a no-op, and a database from a newer app is refused.
        migrate(&mut conn, &newer).unwrap();
        assert!(matches!(
            migrate(&mut conn, &MIGRATIONS[..1]),
            Err(StoreError::FutureSchema(2))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fresh_database_is_at_latest_version() {
        let store = Store::open_in_memory().unwrap();
        let v: i64 = store
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
    }

    #[test]
    fn local_day_matches_calendar() {
        use chrono::{Datelike, TimeZone};
        // Local noon on 2024-03-05 is day 19787 in every time zone.
        let noon = chrono::Local
            .with_ymd_and_hms(2024, 3, 5, 12, 0, 0)
            .unwrap();
        assert_eq!(local_day(noon.timestamp_millis()), 19_787);
        let next = chrono::Local.with_ymd_and_hms(2024, 3, 6, 0, 0, 1).unwrap();
        assert_eq!(local_day(next.timestamp_millis()), 19_788);
        assert_eq!(noon.date_naive().year(), 2024);
        assert!(local_day(0).abs() <= 1);
    }

    #[test]
    #[ignore = "perf: 100k rows"]
    fn list_with_query_on_100k_rows_is_fast() {
        let (dir, path) = temp_db("perf");
        let store = Store::open(&path).unwrap();
        {
            let mut conn = store.conn();
            let tx = conn.transaction().unwrap();
            for i in 0..100_000_i64 {
                let text =
                    format!("sentence number {i} about nothing in particular, 今日は晴れです");
                tx.execute(
                    "INSERT INTO transcripts (id, created_at, status, text, duration_ms) VALUES (?1, ?2, 'ok', ?3, 1000)",
                    params![format!("id{i}"), i, text],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        for q in ["number 77777", "晴れ", "zzz-no-match"] {
            let t = Instant::now();
            let rows = store.list(q, 50, 0).unwrap();
            let n = store.count(q).unwrap();
            let took = t.elapsed();
            println!("query {q:?}: {} rows, count {n}, {took:?}", rows.len());
            assert!(took < Duration::from_millis(50), "{q}: {took:?}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
