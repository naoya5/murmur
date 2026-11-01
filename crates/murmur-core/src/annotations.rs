//! SOT: annotations
//! What: the rules of the experimental screen annotations. The signal types that the adapters
//!       record (screen change per sample, ink strokes, clicks, cursor positions, speech spans),
//!       how the keyframes are chosen from them and capped, how the marks and the spoken segments
//!       are assigned to a keyframe, the text of the pasted summary in Japanese and English, the
//!       two spellings of the picture path, the bundle ids that count as a terminal, and every
//!       tuning constant.
//! Why: the choice of pictures and the wording decide whether an agent understands the bundle.
//!      Keeping them pure (guardrail G1) makes them testable with numbers alone and lets the
//!      constants be tuned from real recordings without touching an adapter.
//! Where: `src-tauri/src/adapters/screen.rs` turns pictures into `ScreenSample`s (16 x 9 cell
//!        brightness), `pointer.rs` records clicks and cursor positions, the drawing layer sends
//!        `InkStroke`s. The controller runs `select_keyframes` on `Effect::PlanKeyframes`, cuts
//!        the audio with `pipeline::segment_at_boundaries` at `boundaries_in_samples`, assigns each
//!        segment with `keyframe_for_segment`, and renders the bundle text with `summary_text`.
//!        While recording it may call `candidates` on the signals so far to learn which samples
//!        to keep as PNGs (the function is pure, so calling it again is cheap). All times are
//!        milliseconds of recording time: the adapters remove paused time before the core sees
//!        it. Tests at the end of this file.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::session::TranscriptText;
use crate::{Language, Millis, SAMPLE_RATE_HZ};

// --- tuning constants (initial values; adjusted from real recordings) ---

/// The screen is divided into this grid; each cell carries its mean brightness (0 to 1).
pub const GRID_COLUMNS: usize = 16;
pub const GRID_ROWS: usize = 9;
pub const GRID_CELLS: usize = GRID_COLUMNS * GRID_ROWS;
/// A cell counts as changed between two samples when its brightness differs by more than this.
pub const CELL_CHANGE_THRESHOLD: f32 = 0.03;
/// A picture is a "change" candidate when at least this share of cells changed since the
/// previous keyframe ...
pub const KEYFRAME_CHANGE_MIN: f32 = 0.10;
/// ... and less than this share changes to the next sample (the screen has settled).
pub const SETTLED_CHANGE_MAX: f32 = 0.02;
/// Ink strokes closer than this in time form one group, unless a click or a big change lies
/// between them.
pub const INK_GROUP_GAP_MS: Millis = 1_500;
/// The cursor must stay within this radius for at least this long, while speech is heard, to
/// count as pointing ("dwell").
pub const DWELL_RADIUS_PT: f32 = 12.0;
pub const DWELL_MIN_MS: Millis = 600;
/// Candidates within this window merge into one keyframe.
pub const MERGE_WINDOW_MS: Millis = 1_000;
/// The contact sheet holds at most this many pictures.
pub const MAX_KEYFRAMES: usize = 6;
/// While recording, at most this many candidate pictures are kept (memory and disk).
pub const MAX_CANDIDATES: usize = 40;
/// A spoken segment belongs to the newest keyframe that starts before the segment start plus
/// this lead: people start talking a little before they draw or click.
pub const SEGMENT_LEAD_MS: Millis = 1_000;

/// Bundle ids of the apps that receive the summary text alone (the picture path is inside it).
/// Every other app receives the picture first, then the text.
pub const TERMINAL_BUNDLE_IDS: [&str; 7] = [
    "com.mitchellh.ghostty",
    "com.googlecode.iterm2",
    "com.apple.Terminal",
    "com.github.wez.wezterm",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "dev.warp.Warp-Stable",
];

// --- signals (recorded by the adapters, in recording time) ---

/// A position on the recorded display as a share of its width and height (0 to 1).
pub type Point = (f32, f32);

/// The name of a frontmost app. `Debug` hides it (guardrail G7: app names stay out of logs).
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AppName(pub String);

impl std::fmt::Debug for AppName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<app>")
    }
}

/// One picture of the screen, reduced to numbers. The picture itself stays in the adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenSample {
    pub t_ms: Millis,
    /// `GRID_CELLS` mean brightness values, row by row. Another length counts as a full change.
    pub cells: Vec<f32>,
    /// The frontmost app when the picture was taken (never its window title).
    pub app: AppName,
}

/// One drawn line, from Shift-drag start to release.
#[derive(Debug, Clone, PartialEq)]
pub struct InkStroke {
    pub t0_ms: Millis,
    pub t1_ms: Millis,
    pub points: Vec<Point>,
}

/// A mouse click in another app.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Click {
    pub t_ms: Millis,
    pub at: Point,
}

/// Where the cursor was at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorSample {
    pub t_ms: Millis,
    pub at: Point,
}

/// A span in which speech was heard (from VAD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechSpan {
    pub start_ms: Millis,
    pub end_ms: Millis,
}

/// Everything recorded during one annotation session, in time order within each list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Signals {
    /// When the recording stopped.
    pub stopped_ms: Millis,
    /// Size of the recorded display in points, to measure the dwell radius.
    pub display_pt: (f32, f32),
    pub samples: Vec<ScreenSample>,
    pub strokes: Vec<InkStroke>,
    pub clicks: Vec<Click>,
    pub cursor: Vec<CursorSample>,
    pub speech: Vec<SpeechSpan>,
}

// --- keyframes and marks (the plan) ---

/// Why a picture was chosen. Several can apply to one keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum KeyframeReason {
    Start,
    Ink,
    Click,
    Change,
    Dwell,
    End,
}

impl KeyframeReason {
    /// Higher wins when candidates merge or the sheet is full.
    pub fn priority(self) -> u8 {
        match self {
            KeyframeReason::Ink => 4,
            KeyframeReason::Click => 3,
            KeyframeReason::Change | KeyframeReason::Dwell => 2,
            KeyframeReason::Start | KeyframeReason::End => 1,
        }
    }
}

/// A picture that may become a keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// When the reason happened (the click, the first stroke, the settled picture, ...).
    pub t_ms: Millis,
    /// Index into `Signals::samples` of the picture to use.
    pub sample: usize,
    pub reasons: Vec<KeyframeReason>,
}

impl Candidate {
    pub fn priority(&self) -> u8 {
        self.reasons.iter().map(|r| r.priority()).max().unwrap_or(0)
    }
}

/// A chosen picture. `n` is its number on the sheet, from 1 in time order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub n: usize,
    pub t_ms: Millis,
    pub sample: usize,
    pub reasons: Vec<KeyframeReason>,
    pub app: AppName,
}

/// Something to draw on a keyframe.
#[derive(Debug, Clone, PartialEq)]
pub enum Mark {
    /// A drawn line with its circled number (`label`, from 1 across the bundle).
    Ink {
        t0_ms: Millis,
        t1_ms: Millis,
        keyframe: usize,
        label: u32,
        points: Vec<Point>,
    },
    Click {
        t_ms: Millis,
        keyframe: usize,
        at: Point,
    },
    /// The cursor rested here while the user spoke.
    Dwell {
        t0_ms: Millis,
        t1_ms: Millis,
        keyframe: usize,
        at: Point,
    },
}

impl Mark {
    pub fn keyframe(&self) -> usize {
        match self {
            Mark::Ink { keyframe, .. }
            | Mark::Click { keyframe, .. }
            | Mark::Dwell { keyframe, .. } => *keyframe,
        }
    }

    /// The moment the mark belongs to in the summary.
    pub fn t_ms(&self) -> Millis {
        match self {
            Mark::Ink { t0_ms, .. } | Mark::Dwell { t0_ms, .. } => *t0_ms,
            Mark::Click { t_ms, .. } => *t_ms,
        }
    }
}

/// The result of `select_keyframes`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Selection {
    pub keyframes: Vec<Keyframe>,
    /// In time order. Empty when there is no keyframe to draw on.
    pub marks: Vec<Mark>,
}

/// Share of cells whose brightness differs by more than `CELL_CHANGE_THRESHOLD` (0 to 1).
/// Grids of different or zero length count as a full change.
pub fn change(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 1.0;
    }
    let changed = a
        .iter()
        .zip(b)
        .filter(|(x, y)| (*x - *y).abs() > CELL_CHANGE_THRESHOLD)
        .count();
    changed as f32 / a.len() as f32
}

/// A rest of the cursor while speaking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DwellSpan {
    pub t0_ms: Millis,
    pub t1_ms: Millis,
    pub at: Point,
}

/// Finds where the cursor stayed within `DWELL_RADIUS_PT` of one spot for `DWELL_MIN_MS` or
/// longer while speech was heard. The spot is where the rest began.
pub fn dwells(
    cursor: &[CursorSample],
    display_pt: (f32, f32),
    speech: &[SpeechSpan],
) -> Vec<DwellSpan> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(anchor) = cursor.get(i) {
        let mut j = i + 1;
        while cursor
            .get(j)
            .is_some_and(|c| distance_pt(anchor.at, c.at, display_pt) <= DWELL_RADIUS_PT)
        {
            j += 1;
        }
        let t1_ms = cursor.get(j - 1).map_or(anchor.t_ms, |c| c.t_ms);
        let long_enough = t1_ms.saturating_sub(anchor.t_ms) >= DWELL_MIN_MS;
        if long_enough && speech_overlaps(speech, anchor.t_ms, t1_ms) {
            out.push(DwellSpan {
                t0_ms: anchor.t_ms,
                t1_ms,
                at: anchor.at,
            });
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn distance_pt(a: Point, b: Point, display_pt: (f32, f32)) -> f32 {
    let dx = (a.0 - b.0) * display_pt.0;
    let dy = (a.1 - b.1) * display_pt.1;
    (dx * dx + dy * dy).sqrt()
}

fn speech_overlaps(speech: &[SpeechSpan], t0_ms: Millis, t1_ms: Millis) -> bool {
    speech
        .iter()
        .any(|s| s.start_ms < t1_ms && s.end_ms > t0_ms)
}

/// Consecutive strokes that belong together: the next starts within `INK_GROUP_GAP_MS` of the
/// previous end, with no click and no big screen change in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InkGroup {
    /// Indices into `Signals::strokes`, inclusive.
    pub first: usize,
    pub last: usize,
    pub t0_ms: Millis,
    pub t1_ms: Millis,
}

pub fn ink_groups(signals: &Signals) -> Vec<InkGroup> {
    let mut groups: Vec<InkGroup> = Vec::new();
    for (i, stroke) in signals.strokes.iter().enumerate() {
        let joins = groups.last().is_some_and(|g| {
            stroke.t0_ms.saturating_sub(g.t1_ms) <= INK_GROUP_GAP_MS
                && !signals
                    .clicks
                    .iter()
                    .any(|c| g.t1_ms < c.t_ms && c.t_ms < stroke.t0_ms)
                && change_between(signals, g.t1_ms, stroke.t0_ms) < KEYFRAME_CHANGE_MIN
        });
        match groups.last_mut() {
            Some(g) if joins => {
                g.last = i;
                g.t1_ms = g.t1_ms.max(stroke.t1_ms);
            }
            _ => groups.push(InkGroup {
                first: i,
                last: i,
                t0_ms: stroke.t0_ms,
                t1_ms: stroke.t1_ms,
            }),
        }
    }
    groups
}

/// Change between the pictures taken before `t0_ms` and before `t1_ms`.
fn change_between(signals: &Signals, t0_ms: Millis, t1_ms: Millis) -> f32 {
    let a = sample_before(&signals.samples, t0_ms);
    let b = sample_before(&signals.samples, t1_ms);
    match (signals.samples.get(a), signals.samples.get(b)) {
        (Some(a), Some(b)) => change(&a.cells, &b.cells),
        _ => 0.0,
    }
}

/// Index of the last sample taken before `t_ms` (the first sample when none is).
fn sample_before(samples: &[ScreenSample], t_ms: Millis) -> usize {
    samples.iter().rposition(|s| s.t_ms < t_ms).unwrap_or(0)
}

/// Index of the first sample taken within `[t0_ms, t1_ms]`, else the last one before `t1_ms`.
fn sample_within(samples: &[ScreenSample], t0_ms: Millis, t1_ms: Millis) -> usize {
    samples
        .iter()
        .position(|s| s.t_ms >= t0_ms && s.t_ms <= t1_ms)
        .unwrap_or_else(|| sample_before(samples, t1_ms))
}

/// Every candidate picture, in time order, before merging and capping.
///
/// Reasons: the first picture (start); the picture before each click; the picture before the
/// end of each ink group; the first picture of each dwell; a settled picture after a big change
/// since the previous candidate; the last picture when it differs from the last candidate.
/// Pure, so the adapter may call it on the signals so far to learn which samples to keep.
pub fn candidates(signals: &Signals) -> Vec<Candidate> {
    let samples = &signals.samples;
    let Some(first) = samples.first() else {
        return Vec::new();
    };
    let mut out = vec![Candidate {
        t_ms: first.t_ms,
        sample: 0,
        reasons: vec![KeyframeReason::Start],
    }];
    for click in &signals.clicks {
        out.push(Candidate {
            t_ms: click.t_ms,
            sample: sample_before(samples, click.t_ms),
            reasons: vec![KeyframeReason::Click],
        });
    }
    for group in ink_groups(signals) {
        out.push(Candidate {
            t_ms: group.t0_ms,
            sample: sample_before(samples, group.t1_ms),
            reasons: vec![KeyframeReason::Ink],
        });
    }
    for dwell in dwells(&signals.cursor, signals.display_pt, &signals.speech) {
        out.push(Candidate {
            t_ms: dwell.t0_ms,
            sample: sample_within(samples, dwell.t0_ms, dwell.t1_ms),
            reasons: vec![KeyframeReason::Dwell],
        });
    }

    // Settled changes, measured from the previous candidate picture in sample order.
    let event_samples: Vec<usize> = out.iter().map(|c| c.sample).collect();
    let mut anchor = 0;
    for i in 0..samples.len() {
        if event_samples.contains(&i) {
            anchor = i;
            continue;
        }
        let (Some(a), Some(here), Some(next)) =
            (samples.get(anchor), samples.get(i), samples.get(i + 1))
        else {
            continue;
        };
        if change(&a.cells, &here.cells) >= KEYFRAME_CHANGE_MIN
            && change(&here.cells, &next.cells) < SETTLED_CHANGE_MAX
        {
            out.push(Candidate {
                t_ms: here.t_ms,
                sample: i,
                reasons: vec![KeyframeReason::Change],
            });
            anchor = i;
        }
    }

    out.sort_by_key(|c| (c.t_ms, c.sample));

    // The end, only when the screen moved on since the last candidate picture.
    let last_index = samples.len() - 1;
    if let (Some(last), Some(prev)) = (
        samples.last(),
        out.last().and_then(|c| samples.get(c.sample)),
    ) && out.last().is_some_and(|c| c.sample != last_index)
        && change(&prev.cells, &last.cells) >= KEYFRAME_CHANGE_MIN
    {
        out.push(Candidate {
            t_ms: signals.stopped_ms.max(last.t_ms),
            sample: last_index,
            reasons: vec![KeyframeReason::End],
        });
    }
    out
}

/// Keeps at most `MAX_CANDIDATES`, dropping the oldest of the lowest priority first. For the
/// adapter, while recording, to bound the pictures it keeps.
pub fn prune_candidates(candidates: &mut Vec<Candidate>) {
    while candidates.len() > MAX_CANDIDATES {
        let Some(lowest) = candidates.iter().map(Candidate::priority).min() else {
            return;
        };
        let Some(i) = candidates.iter().position(|c| c.priority() == lowest) else {
            return;
        };
        candidates.remove(i);
    }
}

/// Chooses the keyframes and assigns the marks.
///
/// 1. Candidates within `MERGE_WINDOW_MS` of each other merge: the picture of the highest
///    priority is used, the earliest time is kept, the reasons are joined.
/// 2. A keyframe whose only reason is the start is dropped when nobody spoke before the next
///    keyframe.
/// 3. Over `MAX_KEYFRAMES`, the lowest priority goes first; among equals, the one that differs
///    least from its neighbours.
/// 4. Each stroke, click and dwell goes to the newest keyframe at or before its moment (the end
///    of a stroke), or to the first keyframe. Strokes are numbered from 1 in time order.
pub fn select_keyframes(signals: &Signals) -> Selection {
    let mut merged = merge_candidates(candidates(signals));
    drop_silent_start(&mut merged, &signals.speech);
    cap_keyframes(&mut merged, &signals.samples);
    let keyframes: Vec<Keyframe> = merged
        .into_iter()
        .enumerate()
        .map(|(i, c)| Keyframe {
            n: i + 1,
            t_ms: c.t_ms,
            app: signals
                .samples
                .get(c.sample)
                .map(|s| s.app.clone())
                .unwrap_or_default(),
            sample: c.sample,
            reasons: c.reasons,
        })
        .collect();
    let marks = assign_marks(signals, &keyframes);
    Selection { keyframes, marks }
}

fn merge_candidates(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut merged: Vec<Candidate> = Vec::new();
    let mut last_t: Option<Millis> = None;
    for c in candidates {
        match (merged.last_mut(), last_t) {
            (Some(open), Some(t)) if c.t_ms.saturating_sub(t) <= MERGE_WINDOW_MS => {
                if c.priority() > open.priority() {
                    open.sample = c.sample;
                }
                for r in c.reasons {
                    if !open.reasons.contains(&r) {
                        open.reasons.push(r);
                    }
                }
                last_t = Some(c.t_ms);
            }
            _ => {
                last_t = Some(c.t_ms);
                merged.push(c);
            }
        }
    }
    merged
}

fn drop_silent_start(keyframes: &mut Vec<Candidate>, speech: &[SpeechSpan]) {
    let Some(next_t) = keyframes.get(1).map(|c| c.t_ms) else {
        return;
    };
    let only_start = keyframes
        .first()
        .is_some_and(|c| c.reasons == [KeyframeReason::Start]);
    let spoke_before = speech.iter().any(|s| s.start_ms < next_t);
    if only_start && !spoke_before {
        keyframes.remove(0);
    }
}

fn cap_keyframes(keyframes: &mut Vec<Candidate>, samples: &[ScreenSample]) {
    while keyframes.len() > MAX_KEYFRAMES {
        let lowest = keyframes.iter().map(Candidate::priority).min().unwrap_or(0);
        let neighbour_change = |i: usize| -> f32 {
            let cells = |j: usize| {
                keyframes
                    .get(j)
                    .and_then(|c| samples.get(c.sample))
                    .map(|s| &s.cells)
            };
            let Some(here) = cells(i) else {
                return 0.0;
            };
            let prev = i.checked_sub(1).and_then(cells).map(|p| change(p, here));
            let next = cells(i + 1).map(|n| change(here, n));
            match (prev, next) {
                (Some(p), Some(n)) => p.min(n),
                (Some(p), None) => p,
                (None, Some(n)) => n,
                (None, None) => 0.0,
            }
        };
        let victim = (0..keyframes.len())
            .filter(|&i| keyframes.get(i).is_some_and(|c| c.priority() == lowest))
            .min_by(|&a, &b| neighbour_change(a).total_cmp(&neighbour_change(b)));
        match victim {
            Some(i) => {
                keyframes.remove(i);
            }
            None => return,
        }
    }
}

/// The keyframe number for a moment: the newest keyframe at or before `t_ms`, else the first.
fn keyframe_at(keyframes: &[Keyframe], t_ms: Millis) -> Option<usize> {
    keyframes
        .iter()
        .rev()
        .find(|k| k.t_ms <= t_ms)
        .or(keyframes.first())
        .map(|k| k.n)
}

fn assign_marks(signals: &Signals, keyframes: &[Keyframe]) -> Vec<Mark> {
    if keyframes.is_empty() {
        return Vec::new();
    }
    let mut marks: Vec<Mark> = Vec::new();
    for (i, stroke) in signals.strokes.iter().enumerate() {
        if let Some(keyframe) = keyframe_at(keyframes, stroke.t1_ms) {
            marks.push(Mark::Ink {
                t0_ms: stroke.t0_ms,
                t1_ms: stroke.t1_ms,
                keyframe,
                label: i as u32 + 1,
                points: stroke.points.clone(),
            });
        }
    }
    for click in &signals.clicks {
        if let Some(keyframe) = keyframe_at(keyframes, click.t_ms) {
            marks.push(Mark::Click {
                t_ms: click.t_ms,
                keyframe,
                at: click.at,
            });
        }
    }
    for dwell in dwells(&signals.cursor, signals.display_pt, &signals.speech) {
        if let Some(keyframe) = keyframe_at(keyframes, dwell.t0_ms) {
            marks.push(Mark::Dwell {
                t0_ms: dwell.t0_ms,
                t1_ms: dwell.t1_ms,
                keyframe,
                at: dwell.at,
            });
        }
    }
    marks.sort_by_key(Mark::t_ms);
    marks
}

// --- audio segments and their keyframe ---

pub fn ms_to_samples(ms: Millis) -> usize {
    (ms as usize).saturating_mul(SAMPLE_RATE_HZ as usize) / 1_000
}

pub fn samples_to_ms(samples: usize) -> Millis {
    (samples as Millis).saturating_mul(1_000) / SAMPLE_RATE_HZ as Millis
}

/// Where the audio is cut so that no segment spans two keyframes: the start of every keyframe
/// but the first, in samples.
pub fn boundaries_in_samples(keyframes: &[Keyframe]) -> Vec<usize> {
    keyframes
        .iter()
        .skip(1)
        .map(|k| ms_to_samples(k.t_ms))
        .collect()
}

/// The keyframe number that receives a segment starting at `start_ms`: the newest keyframe that
/// starts before `start_ms + SEGMENT_LEAD_MS`, or the first keyframe. `None` without keyframes.
pub fn keyframe_for_segment(start_ms: Millis, keyframes: &[Keyframe]) -> Option<usize> {
    let limit = start_ms.saturating_add(SEGMENT_LEAD_MS);
    keyframes
        .iter()
        .rev()
        .find(|k| k.t_ms < limit)
        .or(keyframes.first())
        .map(|k| k.n)
}

// --- the summary text ---

/// A transcribed segment, placed on a keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub start_ms: Millis,
    pub end_ms: Millis,
    pub keyframe: usize,
    pub text: TranscriptText,
}

/// Where the bundle is pasted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteTarget {
    /// Receives the text only; the path inside it is spelled as a dragged file (`\ ` for a space).
    Terminal,
    /// Receives the picture, then the text with the plain path.
    Other,
}

/// Whether a bundle id belongs to a terminal app (case does not matter).
pub fn is_terminal(bundle_id: &str) -> bool {
    TERMINAL_BUNDLE_IDS
        .iter()
        .any(|id| id.eq_ignore_ascii_case(bundle_id.trim()))
}

pub fn paste_target(bundle_id: Option<&str>) -> PasteTarget {
    match bundle_id {
        Some(id) if is_terminal(id) => PasteTarget::Terminal,
        _ => PasteTarget::Other,
    }
}

/// The path as the summary writes it for the target.
pub fn path_for_paste(path: &str, target: PasteTarget) -> String {
    match target {
        PasteTarget::Terminal => path.replace(' ', "\\ "),
        PasteTarget::Other => path.to_owned(),
    }
}

/// What the summary is made of. Chunks and marks are in time order.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary<'a> {
    pub language: Language,
    pub duration_ms: Millis,
    /// The contact sheet path, plain. `None` when no picture could be made (words only).
    pub sheet_path: Option<&'a str>,
    pub target: PasteTarget,
    pub keyframes: &'a [Keyframe],
    pub marks: &'a [Mark],
    pub chunks: &'a [Chunk],
    /// The transcription failed: a note replaces the words, pictures and marks still go out.
    pub transcription_failed: bool,
}

const INK_LABELS: [char; 20] = [
    '①', '②', '③', '④', '⑤', '⑥', '⑦', '⑧', '⑨', '⑩', '⑪', '⑫', '⑬', '⑭', '⑮', '⑯', '⑰', '⑱', '⑲',
    '⑳',
];

/// The circled number of an ink mark, as text.
pub fn ink_label(label: u32) -> String {
    match label
        .checked_sub(1)
        .and_then(|i| INK_LABELS.get(i as usize))
    {
        Some(c) => c.to_string(),
        None => format!("({label})"),
    }
}

/// `m:ss` of a moment in the recording.
pub fn clock(ms: Millis) -> String {
    let s = ms / 1_000;
    format!("{}:{:02}", s / 60, s % 60)
}

struct Wording {
    title: &'static str,
    image: &'static str,
    how_to_read: &'static str,
    numbers_one: &'static str,
    numbers_many: &'static str,
    ink: &'static str,
    click_legend: &'static str,
    dwell_legend: &'static str,
    click: &'static str,
    failed: &'static str,
    originals: &'static str,
    quote: (&'static str, &'static str),
}

const JA: Wording = Wording {
    title: "画面の説明",
    image: "画像",
    how_to_read: "読み方",
    numbers_one: "#1 は画像の番号",
    numbers_many: "#1 から #{n} は画像の番号",
    ink: "①② は手書きの印",
    click_legend: "○ はクリックした所",
    dwell_legend: "◎ はカーソルを止めた所",
    click: "○ クリック",
    failed: "(文字起こしに失敗しました)",
    originals: "元の大きさの画像",
    quote: ("「", "」"),
};

const EN: Wording = Wording {
    title: "Screen notes",
    image: "Image",
    how_to_read: "How to read",
    numbers_one: "#1 is the picture number",
    numbers_many: "#1 to #{n} are the picture numbers",
    ink: "①② are drawn marks",
    click_legend: "○ is a click",
    dwell_legend: "◎ is where the cursor rested",
    click: "○ click",
    failed: "(transcription failed)",
    originals: "Full-size pictures",
    quote: ("\"", "\""),
};

/// The text pasted after the pictures (and written to `summary.md`).
pub fn summary_text(s: &Summary<'_>) -> String {
    let w = match s.language {
        Language::Ja => &JA,
        Language::En => &EN,
    };
    let n = s.keyframes.len();
    let mut out = String::new();

    // Header.
    out.push_str("[Murmur Annotations] ");
    out.push_str(w.title);
    out.push_str(" (");
    out.push_str(&clock(s.duration_ms));
    if s.sheet_path.is_some() && n > 0 {
        match s.language {
            Language::Ja => out.push_str(&format!("、画像 {n} 枚")),
            Language::En => out.push_str(&format!(
                ", {n} {}",
                if n == 1 { "image" } else { "images" }
            )),
        }
    }
    out.push_str(")\n");
    if let Some(path) = s.sheet_path {
        out.push_str(w.image);
        out.push_str(": ");
        out.push_str(&path_for_paste(path, s.target));
        out.push('\n');
    }

    // Legend, only for what the bundle contains.
    if n > 0 {
        let mut parts: Vec<String> = vec![if n == 1 {
            w.numbers_one.to_owned()
        } else {
            w.numbers_many.replace("{n}", &n.to_string())
        }];
        if s.marks.iter().any(|m| matches!(m, Mark::Ink { .. })) {
            parts.push(w.ink.to_owned());
        }
        if s.marks.iter().any(|m| matches!(m, Mark::Click { .. })) {
            parts.push(w.click_legend.to_owned());
        }
        if s.marks.iter().any(|m| matches!(m, Mark::Dwell { .. })) {
            parts.push(w.dwell_legend.to_owned());
        }
        out.push_str(w.how_to_read);
        out.push_str(": ");
        let sep = match s.language {
            Language::Ja => "。",
            Language::En => ". ",
        };
        out.push_str(parts.join(sep).trim_end());
        out.push_str(match s.language {
            Language::Ja => "。\n",
            Language::En => ".\n",
        });
    }

    // One block per keyframe, after one blank line.
    for k in s.keyframes {
        if k.n == 1 || s.keyframes.first().is_some_and(|f| f.n == k.n) {
            out.push('\n');
        }
        out.push_str(&format!("#{} ({})", k.n, clock(k.t_ms)));
        if !k.app.0.trim().is_empty() {
            out.push(' ');
            out.push_str(k.app.0.trim());
        }
        out.push('\n');
        let chunks: Vec<&Chunk> = s.chunks.iter().filter(|c| c.keyframe == k.n).collect();
        let marks: Vec<&Mark> = s.marks.iter().filter(|m| m.keyframe() == k.n).collect();
        if s.transcription_failed {
            let mut line = w.failed.to_owned();
            append_marks(&mut line, &marks, w);
            out.push_str(&line);
            out.push('\n');
            continue;
        }
        // A mark goes after the newest sentence that started before it; the rest stand alone.
        let mut orphans: Vec<&Mark> = Vec::new();
        let mut per_chunk: Vec<Vec<&Mark>> = vec![Vec::new(); chunks.len()];
        for m in marks {
            match chunks.iter().rposition(|c| c.start_ms <= m.t_ms()) {
                Some(i) => {
                    if let Some(list) = per_chunk.get_mut(i) {
                        list.push(m);
                    }
                }
                None => orphans.push(m),
            }
        }
        if !orphans.is_empty() {
            let mut line = String::new();
            append_marks(&mut line, &orphans, w);
            out.push_str(line.trim_start());
            out.push('\n');
        }
        for (c, marks) in chunks.iter().zip(per_chunk) {
            let text = c.text.0.trim();
            if text.is_empty() && marks.is_empty() {
                continue;
            }
            let mut line = String::new();
            if !text.is_empty() {
                line.push_str(w.quote.0);
                line.push_str(text);
                line.push_str(w.quote.1);
            }
            append_marks(&mut line, &marks, w);
            out.push_str(line.trim_start());
            out.push('\n');
        }
    }

    if s.sheet_path.is_some() && n > 0 {
        out.push('\n');
        out.push_str(w.originals);
        out.push_str(": ");
        match s.language {
            Language::Ja if n == 1 => out.push_str("frames/1.png (同じフォルダ)"),
            Language::Ja => {
                out.push_str(&format!("frames/1.png から frames/{n}.png (同じフォルダ)"))
            }
            Language::En if n == 1 => out.push_str("frames/1.png (same folder)"),
            Language::En => out.push_str(&format!("frames/1.png to frames/{n}.png (same folder)")),
        }
        out.push('\n');
    }
    out
}

fn append_marks(line: &mut String, marks: &[&Mark], w: &Wording) {
    for m in marks {
        line.push(' ');
        match m {
            Mark::Ink { label, .. } => line.push_str(&ink_label(*label)),
            Mark::Click { .. } => line.push_str(w.click),
            Mark::Dwell { .. } => line.push('◎'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: (f32, f32) = (1512.0, 982.0);

    fn flat(level: f32) -> Vec<f32> {
        vec![level; GRID_CELLS]
    }

    /// A grid where the first `share` of the cells are bright and the rest dark.
    fn grid(share: f32) -> Vec<f32> {
        let lit = (GRID_CELLS as f32 * share).round() as usize;
        (0..GRID_CELLS)
            .map(|i| if i < lit { 0.9 } else { 0.1 })
            .collect()
    }

    fn sample(t_ms: Millis, cells: Vec<f32>) -> ScreenSample {
        ScreenSample {
            t_ms,
            cells,
            app: AppName("Safari".to_owned()),
        }
    }

    /// Samples every 500 ms from 0 to `until_ms`, all showing the same screen.
    fn steady_samples(until_ms: Millis) -> Vec<ScreenSample> {
        (0..=until_ms / 500)
            .map(|i| sample(i * 500, flat(0.5)))
            .collect()
    }

    fn signals(until_ms: Millis) -> Signals {
        Signals {
            stopped_ms: until_ms,
            display_pt: DISPLAY,
            samples: steady_samples(until_ms),
            ..Signals::default()
        }
    }

    fn stroke(t0_ms: Millis, t1_ms: Millis) -> InkStroke {
        InkStroke {
            t0_ms,
            t1_ms,
            points: vec![(0.4, 0.2), (0.45, 0.25)],
        }
    }

    fn click(t_ms: Millis) -> Click {
        Click {
            t_ms,
            at: (0.8, 0.1),
        }
    }

    fn span(start_ms: Millis, end_ms: Millis) -> SpeechSpan {
        SpeechSpan { start_ms, end_ms }
    }

    fn reasons(selection: &Selection) -> Vec<Vec<KeyframeReason>> {
        selection
            .keyframes
            .iter()
            .map(|k| k.reasons.clone())
            .collect()
    }

    // --- change ---

    #[test]
    fn change_is_the_share_of_cells_over_the_threshold() {
        assert_eq!(change(&flat(0.5), &flat(0.5)), 0.0);
        assert_eq!(change(&flat(0.5), &flat(0.52)), 0.0, "within the threshold");
        assert_eq!(change(&flat(0.5), &flat(0.54)), 1.0);
        let half = change(&grid(0.0), &grid(0.5));
        assert!((half - 0.5).abs() < 0.01, "{half}");
        // Different or empty grids count as a full change.
        assert_eq!(change(&flat(0.5), &flat(0.5)[..10]), 1.0);
        assert_eq!(change(&[], &[]), 1.0);
    }

    // --- dwells ---

    fn cursor_path(points: &[(Millis, f32, f32)]) -> Vec<CursorSample> {
        points
            .iter()
            .map(|(t, x, y)| CursorSample {
                t_ms: *t,
                at: (*x, *y),
            })
            .collect()
    }

    #[test]
    fn dwell_needs_a_still_cursor_and_speech() {
        // 12 pt is 12 / 1512 of the width.
        let near = 10.0 / DISPLAY.0;
        let far = 40.0 / DISPLAY.0;
        let path = cursor_path(&[
            (0, 0.5, 0.5),
            (200, 0.5 + near, 0.5),
            (400, 0.5, 0.5),
            (700, 0.5 + near, 0.5),
            (900, 0.5 + far, 0.5),
            (1_100, 0.5 + far, 0.5),
        ]);
        let found = dwells(&path, DISPLAY, &[span(300, 2_000)]);
        assert_eq!(
            found,
            vec![DwellSpan {
                t0_ms: 0,
                t1_ms: 700,
                at: (0.5, 0.5)
            }]
        );
        // Silence: no dwell. Too short: no dwell.
        assert!(dwells(&path, DISPLAY, &[]).is_empty());
        assert!(dwells(&path, DISPLAY, &[span(800, 2_000)]).is_empty());
        assert!(dwells(&path[..3], DISPLAY, &[span(0, 2_000)]).is_empty());
        // The radius is measured in points: a small share of a large display is far.
        assert!(dwells(&path, (20_000.0, 982.0), &[span(0, 2_000)]).is_empty());
    }

    #[test]
    fn two_dwells_in_a_row_are_separate() {
        let path = cursor_path(&[
            (0, 0.2, 0.2),
            (700, 0.2, 0.2),
            (800, 0.8, 0.8),
            (1_600, 0.8, 0.8),
        ]);
        let found = dwells(&path, DISPLAY, &[span(0, 2_000)]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].at, (0.2, 0.2));
        assert_eq!(found[1].at, (0.8, 0.8));
        assert_eq!(found[1].t0_ms, 800);
    }

    // --- ink groups ---

    #[test]
    fn strokes_group_within_the_gap_unless_a_click_or_change_separates_them() {
        let mut s = signals(20_000);
        s.strokes = vec![
            stroke(1_000, 1_400),
            stroke(2_000, 2_300), // 600 ms after the first: same group
            stroke(4_500, 4_800), // 2.2 s later: new group
            stroke(5_000, 5_200), // a click lies between: new group
            stroke(6_000, 6_200), // the screen changed between: new group
        ];
        s.clicks = vec![click(4_900)];
        for sample in s.samples.iter_mut().filter(|x| x.t_ms >= 5_500) {
            sample.cells = flat(0.9);
        }
        let groups = ink_groups(&s);
        assert_eq!(
            groups
                .iter()
                .map(|g| (g.first, g.last, g.t0_ms, g.t1_ms))
                .collect::<Vec<_>>(),
            vec![
                (0, 1, 1_000, 2_300),
                (2, 2, 4_500, 4_800),
                (3, 3, 5_000, 5_200),
                (4, 4, 6_000, 6_200)
            ]
        );
    }

    // --- candidates and selection ---

    #[test]
    fn no_samples_means_no_keyframes_and_no_marks() {
        let s = Signals {
            stopped_ms: 5_000,
            display_pt: DISPLAY,
            strokes: vec![stroke(1_000, 1_200)],
            clicks: vec![click(2_000)],
            ..Signals::default()
        };
        assert!(candidates(&s).is_empty());
        assert_eq!(select_keyframes(&s), Selection::default());
    }

    #[test]
    fn start_alone_is_kept_when_spoken_over() {
        let mut s = signals(10_000);
        s.speech = vec![span(500, 9_000)];
        let sel = select_keyframes(&s);
        assert_eq!(reasons(&sel), vec![vec![KeyframeReason::Start]]);
        assert_eq!(sel.keyframes[0].n, 1);
        assert_eq!(sel.keyframes[0].t_ms, 0);
        assert_eq!(sel.keyframes[0].sample, 0);
        assert_eq!(sel.keyframes[0].app.0, "Safari");
    }

    #[test]
    fn start_is_dropped_when_nobody_spoke_before_the_next_keyframe() {
        let mut s = signals(10_000);
        s.clicks = vec![click(4_200)];
        s.speech = vec![span(5_000, 9_000)];
        let sel = select_keyframes(&s);
        assert_eq!(reasons(&sel), vec![vec![KeyframeReason::Click]]);
        assert_eq!(sel.keyframes[0].n, 1, "numbering restarts after the drop");
        // With speech before the click the start stays.
        s.speech = vec![span(1_000, 9_000)];
        let sel = select_keyframes(&s);
        assert_eq!(
            reasons(&sel),
            vec![vec![KeyframeReason::Start], vec![KeyframeReason::Click]]
        );
    }

    #[test]
    fn a_click_uses_the_picture_before_it() {
        let mut s = signals(10_000);
        s.clicks = vec![click(4_200)];
        let c = candidates(&s);
        let click_c = c
            .iter()
            .find(|c| c.reasons == [KeyframeReason::Click])
            .unwrap();
        assert_eq!(click_c.t_ms, 4_200);
        assert_eq!(s.samples[click_c.sample].t_ms, 4_000);
    }

    #[test]
    fn an_ink_group_uses_the_picture_before_its_end_and_starts_at_its_first_stroke() {
        let mut s = signals(20_000);
        s.strokes = vec![stroke(8_200, 8_600), stroke(9_000, 9_400)];
        s.speech = vec![span(0, 20_000)];
        let sel = select_keyframes(&s);
        assert_eq!(
            reasons(&sel),
            vec![vec![KeyframeReason::Start], vec![KeyframeReason::Ink]]
        );
        let ink = &sel.keyframes[1];
        assert_eq!(ink.t_ms, 8_200);
        assert_eq!(s.samples[ink.sample].t_ms, 9_000);
        // Both strokes are drawn on the ink keyframe, numbered in order.
        let inks: Vec<(usize, u32)> = sel
            .marks
            .iter()
            .filter_map(|m| match m {
                Mark::Ink {
                    keyframe, label, ..
                } => Some((*keyframe, *label)),
                _ => None,
            })
            .collect();
        assert_eq!(inks, vec![(2, 1), (2, 2)]);
    }

    #[test]
    fn a_settled_change_is_a_candidate_and_a_moving_screen_is_not() {
        let mut s = signals(10_000);
        // The screen flips at 3 s and settles at once.
        for sample in s.samples.iter_mut().filter(|x| x.t_ms >= 3_000) {
            sample.cells = flat(0.9);
        }
        let c = candidates(&s);
        let changes: Vec<Millis> = c
            .iter()
            .filter(|c| c.reasons == [KeyframeReason::Change])
            .map(|c| c.t_ms)
            .collect();
        assert_eq!(changes, vec![3_000]);

        // A screen that keeps moving (video) never settles: no change candidate, but the end
        // differs from the last candidate.
        let mut s = signals(10_000);
        for (i, sample) in s.samples.iter_mut().enumerate() {
            sample.cells = flat(0.1 + 0.08 * i as f32);
        }
        let c = candidates(&s);
        assert!(c.iter().all(|c| c.reasons != [KeyframeReason::Change]));
        assert_eq!(c.last().unwrap().reasons, vec![KeyframeReason::End]);
        assert_eq!(c.last().unwrap().t_ms, 10_000);
    }

    #[test]
    fn the_end_is_a_candidate_only_when_the_screen_moved_on() {
        let s = signals(10_000);
        assert_eq!(candidates(&s).len(), 1, "steady screen: start only");
        let mut s = signals(10_000);
        if let Some(last) = s.samples.last_mut() {
            last.cells = flat(0.9);
        }
        let c = candidates(&s);
        assert_eq!(c.last().unwrap().reasons, vec![KeyframeReason::End]);
        assert_eq!(c.last().unwrap().sample, s.samples.len() - 1);
    }

    #[test]
    fn candidates_within_a_second_merge_keeping_the_best_picture_and_the_earliest_time() {
        let mut s = signals(10_000);
        s.clicks = vec![click(600)];
        s.speech = vec![span(0, 10_000)];
        let sel = select_keyframes(&s);
        assert_eq!(sel.keyframes.len(), 1);
        let k = &sel.keyframes[0];
        assert_eq!(
            k.reasons,
            vec![KeyframeReason::Start, KeyframeReason::Click]
        );
        assert_eq!(k.t_ms, 0, "the earliest time of the group");
        assert_eq!(s.samples[k.sample].t_ms, 500, "the click's picture wins");
        // The click mark lands on the merged keyframe.
        assert_eq!(
            sel.marks,
            vec![Mark::Click {
                t_ms: 600,
                keyframe: 1,
                at: (0.8, 0.1)
            }]
        );
    }

    #[test]
    fn merging_chains_through_the_window() {
        let mut s = signals(10_000);
        s.clicks = vec![click(2_000), click(2_900), click(3_800), click(6_000)];
        s.speech = vec![span(0, 10_000)];
        let sel = select_keyframes(&s);
        let times: Vec<Millis> = sel.keyframes.iter().map(|k| k.t_ms).collect();
        assert_eq!(times, vec![0, 2_000, 6_000]);
    }

    #[test]
    fn more_than_six_keyframes_drop_the_lowest_priority_first() {
        let mut s = signals(40_000);
        s.speech = vec![span(0, 40_000)];
        // Seven clicks, four seconds apart, plus a dwell and a settled change.
        s.clicks = (0..7).map(|i| click(3_000 + i * 4_000)).collect();
        for sample in s.samples.iter_mut().filter(|x| x.t_ms >= 33_000) {
            sample.cells = flat(0.9);
        }
        s.cursor = cursor_path(&[(37_000, 0.3, 0.3), (37_800, 0.3, 0.3)]);
        let all = candidates(&s);
        assert!(all.len() > MAX_KEYFRAMES, "{all:?}");
        let sel = select_keyframes(&s);
        assert_eq!(sel.keyframes.len(), MAX_KEYFRAMES);
        // Start, change and dwell (priority 1 and 2) go before any click (priority 3).
        assert!(
            sel.keyframes
                .iter()
                .all(|k| k.reasons.contains(&KeyframeReason::Click)),
            "{:?}",
            reasons(&sel)
        );
        let numbers: Vec<usize> = sel.keyframes.iter().map(|k| k.n).collect();
        assert_eq!(numbers, vec![1, 2, 3, 4, 5, 6]);
        // Every mark still has a keyframe to live on.
        assert!(
            sel.marks
                .iter()
                .all(|m| (1..=MAX_KEYFRAMES).contains(&m.keyframe()))
        );
    }

    #[test]
    fn among_equal_priorities_the_most_redundant_picture_goes() {
        let mut s = signals(40_000);
        s.speech = vec![span(0, 40_000)];
        // Eight clicks; the pictures before the clicks at 11 s and 15 s are alike, the rest differ.
        // Outside the alike blocks the screen flickers a little every sample, so that no picture
        // settles into a "change" candidate of its own.
        s.clicks = (0..8).map(|i| click(3_000 + i * 4_000)).collect();
        for (i, sample) in s.samples.iter_mut().enumerate() {
            let block = sample.t_ms / 4_000;
            sample.cells = if (2..4).contains(&block) {
                grid(0.5)
            } else {
                let base = match block {
                    0 => 0.1,
                    1 => 0.2,
                    4 => 0.6,
                    5 => 0.7,
                    6 => 0.8,
                    7 => 0.9,
                    _ => 0.95,
                };
                grid(base + if i % 2 == 0 { 0.0 } else { 0.04 })
            };
        }
        let sel = select_keyframes(&s);
        assert_eq!(sel.keyframes.len(), MAX_KEYFRAMES);
        let times: Vec<Millis> = sel.keyframes.iter().map(|k| k.t_ms).collect();
        // Start and end (priority 1) go first; then, among the clicks, one of the two alike
        // pictures (11 s or 15 s) goes before any distinct one.
        assert!(!times.contains(&0));
        assert!(
            times.contains(&11_000) ^ times.contains(&15_000),
            "{times:?}"
        );
    }

    #[test]
    fn prune_keeps_forty_candidates_dropping_old_low_priority_first() {
        let mut list: Vec<Candidate> = (0..50)
            .map(|i| Candidate {
                t_ms: i * 500,
                sample: i as usize,
                reasons: vec![if i % 5 == 0 {
                    KeyframeReason::Click
                } else {
                    KeyframeReason::Change
                }],
            })
            .collect();
        prune_candidates(&mut list);
        assert_eq!(list.len(), MAX_CANDIDATES);
        // Every click survived; the ten oldest changes went.
        assert_eq!(
            list.iter()
                .filter(|c| c.reasons == [KeyframeReason::Click])
                .count(),
            10
        );
        let oldest_change = list
            .iter()
            .find(|c| c.reasons == [KeyframeReason::Change])
            .unwrap();
        assert!(oldest_change.t_ms >= 12 * 500 - 500, "{oldest_change:?}");
        let mut short = list[..3].to_vec();
        prune_candidates(&mut short);
        assert_eq!(short.len(), 3);
    }

    #[test]
    fn marks_go_to_the_newest_keyframe_at_or_before_them() {
        let mut s = signals(30_000);
        s.speech = vec![span(0, 30_000)];
        s.clicks = vec![click(5_000), click(15_000)];
        s.strokes = vec![stroke(4_000, 4_400), stroke(20_000, 20_500)];
        s.cursor = cursor_path(&[(17_000, 0.6, 0.6), (17_900, 0.6, 0.6)]);
        let sel = select_keyframes(&s);
        let times: Vec<Millis> = sel.keyframes.iter().map(|k| k.t_ms).collect();
        // Start 0, ink 4.0 (merges the click at 5.0), click 15.0, dwell 17.0, ink 20.0.
        assert_eq!(times, vec![0, 4_000, 15_000, 17_000, 20_000]);
        let placed: Vec<(Millis, usize)> =
            sel.marks.iter().map(|m| (m.t_ms(), m.keyframe())).collect();
        assert_eq!(
            placed,
            vec![
                (4_000, 2),
                (5_000, 2),
                (15_000, 3),
                (17_000, 4),
                (20_000, 5)
            ]
        );
    }

    #[test]
    fn a_mark_before_the_first_keyframe_goes_to_it() {
        let mut s = signals(10_000);
        s.samples.remove(0); // the first picture arrives at 0.5 s
        s.clicks = vec![click(200)];
        s.speech = vec![span(0, 10_000)];
        let sel = select_keyframes(&s);
        assert_eq!(sel.marks.len(), 1);
        assert_eq!(sel.marks[0].keyframe(), 1);
    }

    // --- segments ---

    fn keyframes_at(times: &[Millis]) -> Vec<Keyframe> {
        times
            .iter()
            .enumerate()
            .map(|(i, t)| Keyframe {
                n: i + 1,
                t_ms: *t,
                sample: i,
                reasons: vec![KeyframeReason::Click],
                app: AppName::default(),
            })
            .collect()
    }

    #[test]
    fn sample_conversions() {
        assert_eq!(ms_to_samples(1_000), SAMPLE_RATE_HZ as usize);
        assert_eq!(ms_to_samples(8_200), 131_200);
        assert_eq!(samples_to_ms(131_200), 8_200);
        assert_eq!(samples_to_ms(ms_to_samples(42_000)), 42_000);
    }

    #[test]
    fn boundaries_are_the_starts_of_the_later_keyframes() {
        let k = keyframes_at(&[0, 8_200, 21_000]);
        assert_eq!(
            boundaries_in_samples(&k),
            vec![ms_to_samples(8_200), ms_to_samples(21_000)]
        );
        assert!(boundaries_in_samples(&k[..1]).is_empty());
        assert!(boundaries_in_samples(&[]).is_empty());
    }

    #[test]
    fn segments_look_one_second_ahead_for_their_keyframe() {
        let k = keyframes_at(&[0, 8_200, 21_000]);
        assert_eq!(keyframe_for_segment(0, &k), Some(1));
        assert_eq!(
            keyframe_for_segment(7_000, &k),
            Some(1),
            "8.2 is not before 8.0"
        );
        assert_eq!(
            keyframe_for_segment(7_300, &k),
            Some(2),
            "spoken just before drawing"
        );
        assert_eq!(keyframe_for_segment(8_200, &k), Some(2));
        assert_eq!(keyframe_for_segment(19_999, &k), Some(2));
        assert_eq!(
            keyframe_for_segment(20_000, &k),
            Some(2),
            "21.0 is not before 21.0"
        );
        assert_eq!(keyframe_for_segment(20_001, &k), Some(3));
        assert_eq!(keyframe_for_segment(60_000, &k), Some(3));
        // Before the first keyframe: the first one.
        let late = keyframes_at(&[3_000, 9_000]);
        assert_eq!(keyframe_for_segment(500, &late), Some(1));
        assert_eq!(keyframe_for_segment(500, &[]), None);
    }

    #[test]
    fn every_segment_gets_exactly_one_keyframe() {
        let k = keyframes_at(&[0, 5_000, 12_300, 40_000]);
        let starts = [
            0, 1_000, 4_100, 4_000, 11_200, 11_400, 30_000, 39_000, 39_001, 50_000,
        ];
        let mut per_keyframe = vec![0; k.len() + 1];
        for start in starts {
            let n = keyframe_for_segment(start, &k).unwrap();
            per_keyframe[n] += 1;
        }
        assert_eq!(per_keyframe.iter().sum::<usize>(), starts.len());
        assert_eq!(per_keyframe, vec![0, 3, 2, 3, 2]);
    }

    // --- terminals and paths ---

    #[test]
    fn terminal_bundle_ids() {
        for id in TERMINAL_BUNDLE_IDS {
            assert!(is_terminal(id), "{id}");
        }
        assert!(is_terminal("COM.APPLE.TERMINAL"));
        assert!(is_terminal(" com.mitchellh.ghostty "));
        assert!(!is_terminal("com.apple.Safari"));
        assert!(!is_terminal(""));
        assert_eq!(
            paste_target(Some("com.googlecode.iterm2")),
            PasteTarget::Terminal
        );
        assert_eq!(paste_target(Some("com.apple.Notes")), PasteTarget::Other);
        assert_eq!(paste_target(None), PasteTarget::Other);
    }

    #[test]
    fn the_path_escapes_spaces_only_for_terminals() {
        let path = "/Users/me/Library/Application Support/io.github.naoya5.murmur/annotations/x/contact-sheet.png";
        assert_eq!(
            path_for_paste(path, PasteTarget::Terminal),
            "/Users/me/Library/Application\\ Support/io.github.naoya5.murmur/annotations/x/contact-sheet.png"
        );
        assert_eq!(path_for_paste(path, PasteTarget::Other), path);
    }

    // --- summary ---

    fn chunk(start_ms: Millis, end_ms: Millis, keyframe: usize, text: &str) -> Chunk {
        Chunk {
            start_ms,
            end_ms,
            keyframe,
            text: TranscriptText(text.to_owned()),
        }
    }

    fn example_keyframes() -> Vec<Keyframe> {
        let mut k = keyframes_at(&[0, 8_000, 21_000]);
        for key in &mut k {
            key.app = AppName("Safari".to_owned());
        }
        k
    }

    fn example_marks() -> Vec<Mark> {
        vec![
            Mark::Click {
                t_ms: 1_300,
                keyframe: 1,
                at: (0.8, 0.12),
            },
            Mark::Ink {
                t0_ms: 8_200,
                t1_ms: 9_100,
                keyframe: 2,
                label: 1,
                points: vec![(0.41, 0.22)],
            },
            Mark::Ink {
                t0_ms: 10_500,
                t1_ms: 11_000,
                keyframe: 2,
                label: 2,
                points: vec![(0.5, 0.5)],
            },
            Mark::Dwell {
                t0_ms: 21_000,
                t1_ms: 22_100,
                keyframe: 3,
                at: (0.55, 0.6),
            },
        ]
    }

    const SHEET: &str = "/Users/me/Library/Application Support/io.github.naoya5.murmur/annotations/20261102-093015-7f3c/contact-sheet.png";

    #[test]
    fn japanese_summary_for_a_terminal() {
        let keyframes = example_keyframes();
        let marks = example_marks();
        let chunks = vec![
            chunk(0, 2_900, 1, "この保存ボタンを押したら、"),
            chunk(7_400, 9_800, 2, "一覧の右側が欠けます。"),
            chunk(10_000, 12_000, 2, "こっちの行も同じです。"),
            chunk(20_500, 24_000, 3, "設定の画面では問題ありません。"),
        ];
        let text = summary_text(&Summary {
            language: Language::Ja,
            duration_ms: 42_000,
            sheet_path: Some(SHEET),
            target: PasteTarget::Terminal,
            keyframes: &keyframes,
            marks: &marks,
            chunks: &chunks,
            transcription_failed: false,
        });
        let expected = "\
[Murmur Annotations] 画面の説明 (0:42、画像 3 枚)
画像: /Users/me/Library/Application\\ Support/io.github.naoya5.murmur/annotations/20261102-093015-7f3c/contact-sheet.png
読み方: #1 から #3 は画像の番号。①② は手書きの印。○ はクリックした所。◎ はカーソルを止めた所。

#1 (0:00) Safari
「この保存ボタンを押したら、」 ○ クリック
#2 (0:08) Safari
「一覧の右側が欠けます。」 ①
「こっちの行も同じです。」 ②
#3 (0:21) Safari
「設定の画面では問題ありません。」 ◎

元の大きさの画像: frames/1.png から frames/3.png (同じフォルダ)
";
        assert_eq!(text, expected);
    }

    #[test]
    fn english_summary_for_another_app_keeps_the_plain_path() {
        let keyframes = example_keyframes();
        let marks = example_marks();
        let chunks = vec![
            chunk(0, 2_900, 1, "After I press save,"),
            chunk(7_400, 9_800, 2, "the right side of the list is cut off."),
            chunk(10_000, 12_000, 2, "Same for this row."),
            chunk(20_500, 24_000, 3, "The settings screen is fine."),
        ];
        let text = summary_text(&Summary {
            language: Language::En,
            duration_ms: 42_000,
            sheet_path: Some(SHEET),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &marks,
            chunks: &chunks,
            transcription_failed: false,
        });
        let expected = "\
[Murmur Annotations] Screen notes (0:42, 3 images)
Image: /Users/me/Library/Application Support/io.github.naoya5.murmur/annotations/20261102-093015-7f3c/contact-sheet.png
How to read: #1 to #3 are the picture numbers. ①② are drawn marks. ○ is a click. ◎ is where the cursor rested.

#1 (0:00) Safari
\"After I press save,\" ○ click
#2 (0:08) Safari
\"the right side of the list is cut off.\" ①
\"Same for this row.\" ②
#3 (0:21) Safari
\"The settings screen is fine.\" ◎

Full-size pictures: frames/1.png to frames/3.png (same folder)
";
        assert_eq!(text, expected);
    }

    #[test]
    fn summary_with_one_picture_no_marks_and_no_words() {
        let keyframes = example_keyframes()[..1].to_vec();
        let text = summary_text(&Summary {
            language: Language::Ja,
            duration_ms: 5_000,
            sheet_path: Some("/tmp/sheet.png"),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &[],
            chunks: &[],
            transcription_failed: false,
        });
        assert_eq!(
            text,
            "[Murmur Annotations] 画面の説明 (0:05、画像 1 枚)\n画像: /tmp/sheet.png\n読み方: #1 は画像の番号。\n\n#1 (0:00) Safari\n\n元の大きさの画像: frames/1.png (同じフォルダ)\n"
        );
        let text = summary_text(&Summary {
            language: Language::En,
            duration_ms: 65_000,
            sheet_path: Some("/tmp/sheet.png"),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &[],
            chunks: &[],
            transcription_failed: false,
        });
        assert!(text.starts_with("[Murmur Annotations] Screen notes (1:05, 1 image)\n"));
        assert!(text.ends_with("Full-size pictures: frames/1.png (same folder)\n"));
    }

    #[test]
    fn failed_transcription_writes_a_note_and_keeps_the_marks() {
        let keyframes = example_keyframes();
        let marks = example_marks();
        let text = summary_text(&Summary {
            language: Language::Ja,
            duration_ms: 42_000,
            sheet_path: Some("/tmp/sheet.png"),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &marks,
            chunks: &[],
            transcription_failed: true,
        });
        assert!(text.contains("#1 (0:00) Safari\n(文字起こしに失敗しました) ○ クリック\n"));
        assert!(text.contains("#2 (0:08) Safari\n(文字起こしに失敗しました) ① ②\n"));
        assert!(text.contains("#3 (0:21) Safari\n(文字起こしに失敗しました) ◎\n"));
        let text = summary_text(&Summary {
            language: Language::En,
            duration_ms: 42_000,
            sheet_path: Some("/tmp/sheet.png"),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &marks,
            chunks: &[],
            transcription_failed: true,
        });
        assert!(text.contains("\n(transcription failed) ○ click\n"));
    }

    #[test]
    fn marks_without_a_sentence_stand_on_their_own_line() {
        let keyframes = example_keyframes();
        let marks = example_marks();
        // Only the last keyframe has words; the click and the strokes have none before them.
        let chunks = vec![chunk(20_500, 24_000, 3, "こちらです。")];
        let text = summary_text(&Summary {
            language: Language::Ja,
            duration_ms: 42_000,
            sheet_path: Some("/tmp/sheet.png"),
            target: PasteTarget::Other,
            keyframes: &keyframes,
            marks: &marks,
            chunks: &chunks,
            transcription_failed: false,
        });
        assert!(text.contains("#1 (0:00) Safari\n○ クリック\n#2 (0:08) Safari\n① ②\n#3 (0:21) Safari\n「こちらです。」 ◎\n"));
    }

    #[test]
    fn words_only_summary_has_no_path_and_no_picture_count() {
        let keyframes = example_keyframes();
        let chunks = vec![chunk(0, 2_900, 1, "hello")];
        let text = summary_text(&Summary {
            language: Language::En,
            duration_ms: 42_000,
            sheet_path: None,
            target: PasteTarget::Terminal,
            keyframes: &keyframes,
            marks: &[],
            chunks: &chunks,
            transcription_failed: false,
        });
        assert!(text.starts_with("[Murmur Annotations] Screen notes (0:42)\nHow to read:"));
        assert!(!text.contains("Image:"));
        assert!(!text.contains("Full-size"));
        assert!(text.contains("\"hello\"\n"));
    }

    #[test]
    fn ink_labels_and_clock() {
        assert_eq!(ink_label(1), "①");
        assert_eq!(ink_label(20), "⑳");
        assert_eq!(ink_label(21), "(21)");
        assert_eq!(ink_label(0), "(0)");
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(42_000), "0:42");
        assert_eq!(clock(42_999), "0:42");
        assert_eq!(clock(605_000), "10:05");
    }

    #[test]
    fn app_names_do_not_print_in_debug() {
        let s = format!("{:?}", AppName("Secret Bank".to_owned()));
        assert!(!s.contains("Secret"));
    }
}
