//! Annotation bundle bookkeeping: the bundle folder, its file modes, the recording clock, the
//! bundle files (`summary.md`, `annotation.json`), retention and the size cap.
//!
//! The cleanup runs on blocking threads after the paste (G9). Nothing here creates the
//! annotations root on its own: it appears with the first bundle, so while the feature is off
//! (or was never used) the cleanup finds no folder and returns at once.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use murmur_core::annotations::{Chunk, Keyframe, Mark};

use crate::adapters::store::{AnnotationStatus, Store};

/// The whole annotations folder stays under this size; older bundles go first.
pub const MAX_TOTAL_BYTES: u64 = 1 << 30;
/// Bundle folders are private to the user.
pub const FOLDER_MODE: u32 = 0o700;
pub const FILE_MODE: u32 = 0o600;
/// Folders that no row refers to (a crash mid-recording) are kept this long after their change.
const ORPHAN_GRACE_MS: i64 = 24 * 60 * 60 * 1_000;

/// What a cleanup did. Counts and bytes only (G7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CleanupReport {
    pub expired: usize,
    pub orphans: usize,
    pub over_cap: usize,
    pub freed_bytes: u64,
}

/// `YYYYMMDD-HHMMSS-xxxx` in local time, `xxxx` from the row id, so folders sort by time.
pub fn folder_name(created_at_ms: i64, id: &str) -> String {
    use chrono::TimeZone;
    let stamp = chrono::Local
        .timestamp_millis_opt(created_at_ms)
        .single()
        .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| "00000000-000000".to_owned());
    let tail: String = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(4)
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{stamp}-{tail:0>4}")
}

// --- the recording clock ---

/// Recording time: milliseconds since the hotkey, with the cancel countdowns taken out. The
/// adapters report wall time since the same origin; the drawing layer reports epoch
/// milliseconds (`Date.now()`); both are converted here.
#[derive(Debug, Clone)]
pub struct RecordingClock {
    origin: Instant,
    origin_epoch_ms: f64,
    /// Closed pauses as wall-time ranges since the origin.
    pauses: Vec<(u64, u64)>,
    open_pause: Option<u64>,
}

impl RecordingClock {
    pub fn new(origin: Instant, origin_epoch_ms: f64) -> RecordingClock {
        RecordingClock {
            origin,
            origin_epoch_ms,
            pauses: Vec::new(),
            open_pause: None,
        }
    }

    /// Wall time since the origin, now.
    pub fn wall_now(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    pub fn origin(&self) -> Instant {
        self.origin
    }

    pub fn origin_epoch_ms(&self) -> f64 {
        self.origin_epoch_ms
    }

    pub fn pause_at(&mut self, wall_ms: u64) {
        if self.open_pause.is_none() {
            self.open_pause = Some(wall_ms);
        }
    }

    pub fn resume_at(&mut self, wall_ms: u64) {
        if let Some(start) = self.open_pause.take() {
            self.pauses.push((start, wall_ms.max(start)));
        }
    }

    /// Recording time of a wall moment, or `None` while the recording was paused then.
    pub fn recording_from_wall(&self, wall_ms: u64) -> Option<u64> {
        if self.open_pause.is_some_and(|start| wall_ms >= start) {
            return None;
        }
        let mut removed = 0u64;
        for (start, end) in &self.pauses {
            if wall_ms >= *end {
                removed += end - start;
            } else if wall_ms >= *start {
                return None;
            }
        }
        Some(wall_ms.saturating_sub(removed))
    }

    /// Recording time of an epoch moment (the drawing layer's clock).
    pub fn recording_from_epoch(&self, epoch_ms: f64) -> Option<u64> {
        let wall = epoch_ms - self.origin_epoch_ms;
        if wall < 0.0 {
            return Some(0);
        }
        self.recording_from_wall(wall.round() as u64)
    }

    /// Recording time now.
    pub fn recording_now(&self) -> u64 {
        let wall = self.wall_now();
        self.recording_from_wall(wall).unwrap_or_else(|| {
            let start = self.open_pause.unwrap_or(wall);
            self.recording_from_wall(start.saturating_sub(1))
                .unwrap_or(0)
        })
    }
}

/// The kept candidate nearest to `sample`: the newest at or before it, else the oldest after.
pub fn nearest_kept(sample: usize, kept: &[usize]) -> Option<usize> {
    kept.iter()
        .copied()
        .filter(|k| *k <= sample)
        .max()
        .or_else(|| kept.iter().copied().filter(|k| *k > sample).min())
}

// --- the bundle files ---

/// Writes `summary.md` and `annotation.json` into the bundle folder (mode 0600).
pub fn write_bundle_files(
    dir: &Path,
    summary: &str,
    json: &serde_json::Value,
) -> Result<(), std::io::Error> {
    write_private(&dir.join("summary.md"), summary.as_bytes())?;
    let text = serde_json::to_string_pretty(json)?;
    write_private(&dir.join("annotation.json"), text.as_bytes())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    std::fs::write(path, bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))
}

/// What `annotation.json` records about the display.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayFacts {
    pub width_pt: f64,
    pub height_pt: f64,
    pub scale: f64,
}

/// The machine-readable record of a bundle (spec 7.4). Positions are shares of the display.
#[allow(clippy::too_many_arguments)]
pub fn annotation_json(
    created_at_ms: i64,
    duration_ms: u64,
    language: &str,
    model_id: Option<&str>,
    display: Option<DisplayFacts>,
    keyframes: &[Keyframe],
    marks: &[Mark],
    chunks: &[Chunk],
    status: AnnotationStatus,
) -> serde_json::Value {
    use chrono::TimeZone;
    use serde_json::json;
    let created_at = chrono::Local
        .timestamp_millis_opt(created_at_ms)
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default();
    // Four decimals of a share of the display: well under a pixel, and stable through JSON.
    let share = |v: f32| (f64::from(v) * 10_000.0).round() / 10_000.0;
    let point = |p: (f32, f32)| json!([share(p.0), share(p.1)]);
    let keyframes: Vec<serde_json::Value> = keyframes
        .iter()
        .map(|k| {
            let reasons: Vec<String> = k
                .reasons
                .iter()
                .map(|r| format!("{r:?}").to_lowercase())
                .collect();
            json!({
                "n": k.n,
                "t_ms": k.t_ms,
                "reasons": reasons,
                "app": k.app.0,
                "file": format!("frames/{}.png", k.n),
            })
        })
        .collect();
    let marks: Vec<serde_json::Value> = marks
        .iter()
        .map(|m| match m {
            Mark::Ink {
                t0_ms,
                t1_ms,
                keyframe,
                label,
                points,
            } => json!({
                "kind": "ink",
                "t0_ms": t0_ms,
                "t1_ms": t1_ms,
                "keyframe": keyframe,
                "label": label,
                "points": points.iter().map(|p| point(*p)).collect::<Vec<_>>(),
            }),
            Mark::Click { t_ms, keyframe, at } => json!({
                "kind": "click",
                "t_ms": t_ms,
                "keyframe": keyframe,
                "at": point(*at),
            }),
            Mark::Dwell {
                t0_ms,
                t1_ms,
                keyframe,
                at,
            } => json!({
                "kind": "dwell",
                "t0_ms": t0_ms,
                "t1_ms": t1_ms,
                "keyframe": keyframe,
                "at": point(*at),
            }),
        })
        .collect();
    let chunks: Vec<serde_json::Value> = chunks
        .iter()
        .map(|c| {
            json!({
                "start_ms": c.start_ms,
                "end_ms": c.end_ms,
                "keyframe": c.keyframe,
                "text": c.text.0,
            })
        })
        .collect();
    let status = match status {
        AnnotationStatus::Ok => "ok",
        AnnotationStatus::TextOnly => "text_only",
        AnnotationStatus::Failed => "failed",
    };
    json!({
        "version": 1,
        "created_at": created_at,
        "duration_ms": duration_ms,
        "language": language,
        "model_id": model_id,
        "display": display.map(|d| json!({
            "width_pt": d.width_pt,
            "height_pt": d.height_pt,
            "scale": d.scale,
        })),
        "keyframes": keyframes,
        "marks": marks,
        "chunks": chunks,
        "status": status,
    })
}

/// Deletes what retention and the size cap no longer allow. `root` is the annotations folder;
/// when it does not exist nothing happens and it is not created.
///
/// 1. Bundles older than `retention_days` (rows and folders, whatever their status).
/// 2. Folders no row refers to, once they are a day old (an interrupted recording).
/// 3. The oldest bundles until the folder is under `MAX_TOTAL_BYTES`.
pub fn cleanup(store: &Store, root: &Path, retention_days: u32, now_ms: i64) -> CleanupReport {
    let mut report = CleanupReport::default();
    if !root.is_dir() {
        return report;
    }
    let cutoff = now_ms - i64::from(retention_days) * 86_400_000;

    match store.purge_annotations_before(cutoff) {
        Ok(folders) => {
            for folder in folders {
                report.freed_bytes += remove_bundle(root, &folder);
                report.expired += 1;
            }
        }
        Err(error) => tracing::error!(%error, "annotation retention purge failed"),
    }

    let rows = match store.list_annotations_oldest_first() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(%error, "could not list the annotation bundles");
            return report;
        }
    };

    for orphan in orphan_folders(
        root,
        rows.iter().map(|r| r.folder.as_str()),
        now_ms - ORPHAN_GRACE_MS,
    ) {
        report.freed_bytes += remove_bundle(root, &orphan);
        report.orphans += 1;
    }

    let mut total: u64 = rows.iter().map(|r| dir_size(&root.join(&r.folder))).sum();
    for row in rows {
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        if let Err(error) = store.delete_annotation(&row.id) {
            tracing::error!(%error, "could not delete an annotation row");
            break;
        }
        let freed = remove_bundle(root, &row.folder);
        total = total.saturating_sub(freed);
        report.freed_bytes += freed;
        report.over_cap += 1;
    }

    if report != CleanupReport::default() {
        tracing::info!(
            expired = report.expired,
            orphans = report.orphans,
            over_cap = report.over_cap,
            freed_bytes = report.freed_bytes,
            "annotation bundles cleaned up"
        );
    }
    report
}

/// Folders directly under `root` that no row names and that changed before `older_than_ms`.
fn orphan_folders<'a>(
    root: &Path,
    known: impl Iterator<Item = &'a str>,
    older_than_ms: i64,
) -> Vec<String> {
    let known: std::collections::HashSet<&str> = known.collect();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if known.contains(name.as_str()) {
            continue;
        }
        let changed_ms = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(i64::MAX, |d| d.as_millis() as i64);
        if changed_ms < older_than_ms {
            out.push(name);
        }
    }
    out.sort();
    out
}

/// Removes one bundle folder under `root`; returns the bytes it held. A name that would leave
/// `root` (a corrupt row) is refused.
fn remove_bundle(root: &Path, folder: &str) -> u64 {
    let Some(path) = bundle_path(root, folder) else {
        tracing::warn!("refused to remove a bundle folder outside the annotations root");
        return 0;
    };
    let size = dir_size(&path);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => size,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            tracing::warn!(%error, "could not remove a bundle folder");
            0
        }
    }
}

/// `root/<folder>` when `folder` is a plain name (no separators, not `.` or `..`).
pub fn bundle_path(root: &Path, folder: &str) -> Option<PathBuf> {
    let plain =
        !folder.is_empty() && folder != "." && folder != ".." && !folder.contains(['/', '\\']);
    plain.then(|| root.join(folder))
}

/// Bytes of the regular files under `path` (one level of subfolders is enough for a bundle,
/// but it walks all the way for safety).
pub fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::store::{AnnotationRow, AnnotationStatus};

    fn row(id: &str, created_at_ms: i64) -> AnnotationRow {
        AnnotationRow {
            id: id.to_owned(),
            created_at_ms,
            status: AnnotationStatus::Ok,
            folder: folder_name(created_at_ms, id),
            duration_ms: 5_000,
            keyframes: 2,
            marks: 1,
            language: None,
            model_id: None,
            latency_ms: None,
            asr_ms: None,
            compose_ms: None,
            error: None,
        }
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("murmur-bundles-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_bundle(root: &Path, folder: &str, bytes: usize) {
        let dir = root.join(folder);
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        std::fs::write(dir.join("contact-sheet.png"), vec![0u8; bytes]).unwrap();
        std::fs::write(dir.join("frames").join("1.png"), vec![0u8; 10]).unwrap();
    }

    const DAY: i64 = 86_400_000;

    #[test]
    fn folder_names_sort_by_time_and_carry_four_id_characters() {
        let a = folder_name(1_700_000_000_000, "a1b2c3d4-0000");
        let b = folder_name(1_700_000_001_000, "ZZ-9-x");
        assert_eq!(a.len(), "20231114-000000-xxxx".len());
        assert!(a.ends_with("-a1b2"));
        assert!(b.ends_with("-zz9x"));
        assert!(a < b);
        assert!(folder_name(1_700_000_000_000, "").ends_with("-0000"));
    }

    #[test]
    fn bundle_paths_stay_under_the_root() {
        let root = Path::new("/tmp/annotations");
        assert_eq!(
            bundle_path(root, "20261102-093015-7f3c"),
            Some(root.join("20261102-093015-7f3c"))
        );
        for bad in ["", ".", "..", "../x", "a/b", "a\\b"] {
            assert_eq!(bundle_path(root, bad), None, "{bad}");
        }
    }

    #[test]
    fn missing_root_stays_missing() {
        let store = Store::open_in_memory().unwrap();
        let root = scratch().join("annotations");
        assert_eq!(cleanup(&store, &root, 7, 0), CleanupReport::default());
        assert!(!root.exists());
    }

    #[test]
    fn expired_bundles_are_removed_with_their_folders() {
        let store = Store::open_in_memory().unwrap();
        let root = scratch();
        let now = 100 * DAY;
        let old = row("old", now - 8 * DAY);
        let fresh = row("fresh", now - 6 * DAY);
        for r in [&old, &fresh] {
            store.insert_annotation(r).unwrap();
            write_bundle(&root, &r.folder, 100);
        }
        let report = cleanup(&store, &root, 7, now);
        assert_eq!(report.expired, 1);
        assert_eq!(report.freed_bytes, 110);
        assert!(!root.join(&old.folder).exists());
        assert!(root.join(&fresh.folder).exists());
        let left = store.list_annotations_oldest_first().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "fresh");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn orphan_folders_go_after_a_day_and_files_are_left_alone() {
        let store = Store::open_in_memory().unwrap();
        let root = scratch();
        write_bundle(&root, "19990101-000000-dead", 50);
        std::fs::write(root.join("notes.txt"), b"keep").unwrap();
        // The folder was just written, so it is younger than the grace period: kept.
        let report = cleanup(&store, &root, 7, 0);
        assert_eq!(report.orphans, 0);
        assert!(root.join("19990101-000000-dead").exists());
        // A day later it is an orphan.
        let later = now_ms() + 2 * DAY;
        let report = cleanup(&store, &root, 7, later);
        assert_eq!(report.orphans, 1);
        assert!(!root.join("19990101-000000-dead").exists());
        assert!(root.join("notes.txt").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn under_the_cap_nothing_is_removed() {
        let store = Store::open_in_memory().unwrap();
        let root = scratch();
        let now = 100 * DAY;
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            let r = row(id, now - 3 * DAY + i as i64 * 1_000);
            store.insert_annotation(&r).unwrap();
            write_bundle(&root, &r.folder, 100);
        }
        let report = cleanup(&store, &root, 7, now);
        assert_eq!(report, CleanupReport::default());
        assert_eq!(store.list_annotations_oldest_first().unwrap().len(), 3);
        assert_eq!(dir_size(&root), 330);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn over_cap_order_is_oldest_first() {
        // The removal order is what matters; the cap itself is a constant.
        let store = Store::open_in_memory().unwrap();
        let now = 100 * DAY;
        for (i, id) in ["c", "a", "b"].iter().enumerate() {
            store
                .insert_annotation(&row(id, now - 3 * DAY + [2_000, 0, 1_000][i]))
                .unwrap();
        }
        let ids: Vec<String> = store
            .list_annotations_oldest_first()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn the_recording_clock_removes_pauses() {
        let mut clock = RecordingClock::new(Instant::now(), 1_000_000.0);
        assert_eq!(clock.recording_from_wall(500), Some(500));
        clock.pause_at(1_000);
        assert_eq!(clock.recording_from_wall(900), Some(900));
        assert_eq!(
            clock.recording_from_wall(1_500),
            None,
            "inside the open pause"
        );
        clock.resume_at(1_600);
        assert_eq!(
            clock.recording_from_wall(1_500),
            None,
            "inside the closed pause"
        );
        assert_eq!(clock.recording_from_wall(1_600), Some(1_000));
        assert_eq!(clock.recording_from_wall(2_600), Some(2_000));
        // A second pause stacks.
        clock.pause_at(3_000);
        clock.resume_at(3_400);
        assert_eq!(clock.recording_from_wall(4_000), Some(3_000));
        // Epoch milliseconds go through the origin; before the origin clamps to zero.
        assert_eq!(
            clock.recording_from_epoch(1_000_000.0 + 4_000.0),
            Some(3_000)
        );
        assert_eq!(clock.recording_from_epoch(999_000.0), Some(0));
        assert_eq!(clock.recording_from_epoch(1_000_000.0 + 1_200.0), None);
    }

    #[test]
    fn nearest_kept_prefers_the_newest_before() {
        assert_eq!(nearest_kept(10, &[2, 7, 12]), Some(7));
        assert_eq!(nearest_kept(7, &[2, 7, 12]), Some(7));
        assert_eq!(nearest_kept(1, &[2, 7, 12]), Some(2));
        assert_eq!(nearest_kept(5, &[]), None);
    }

    #[test]
    fn bundle_files_are_private_and_the_json_follows_the_record_shape() {
        use murmur_core::annotations::{AppName, KeyframeReason};
        use murmur_core::session::TranscriptText;
        let dir = scratch();
        let keyframes = vec![Keyframe {
            n: 1,
            t_ms: 0,
            sample: 0,
            reasons: vec![KeyframeReason::Start, KeyframeReason::Click],
            app: AppName("Safari".into()),
        }];
        let marks = vec![
            Mark::Click {
                t_ms: 1_300,
                keyframe: 1,
                at: (0.8, 0.12),
            },
            Mark::Ink {
                t0_ms: 2_000,
                t1_ms: 2_500,
                keyframe: 1,
                label: 1,
                points: vec![(0.1, 0.2), (0.3, 0.4)],
            },
            Mark::Dwell {
                t0_ms: 3_000,
                t1_ms: 3_700,
                keyframe: 1,
                at: (0.5, 0.5),
            },
        ];
        let chunks = vec![Chunk {
            start_ms: 0,
            end_ms: 2_900,
            keyframe: 1,
            text: TranscriptText("hello".into()),
        }];
        let json = annotation_json(
            1_700_000_000_000,
            42_000,
            "ja",
            Some("haas-ja-v1.0-q8_0"),
            Some(DisplayFacts {
                width_pt: 1512.0,
                height_pt: 982.0,
                scale: 2.0,
            }),
            &keyframes,
            &marks,
            &chunks,
            AnnotationStatus::Ok,
        );
        assert_eq!(json["version"], 1);
        assert_eq!(json["status"], "ok");
        assert_eq!(
            json["keyframes"][0]["reasons"],
            serde_json::json!(["start", "click"])
        );
        assert_eq!(json["keyframes"][0]["file"], "frames/1.png");
        assert_eq!(json["marks"][0]["kind"], "click");
        assert_eq!(json["marks"][1]["label"], 1);
        assert_eq!(json["marks"][2]["kind"], "dwell");
        assert_eq!(json["chunks"][0]["text"], "hello");
        assert_eq!(json["display"]["scale"], 2.0);
        assert!(
            json["created_at"]
                .as_str()
                .unwrap()
                .starts_with("2023-11-1")
        );
        write_bundle_files(&dir, "summary", &json).unwrap();
        for name in ["summary.md", "annotation.json"] {
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("annotation.json")).unwrap())
                .unwrap();
        assert_eq!(back, json);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
