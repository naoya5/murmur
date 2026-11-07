//! SOT: annotations
//! What: the rules of the experimental screen annotations. The signals the adapters record
//!       (screen samples as cell brightness, ink strokes, speech spans), how the strokes form
//!       groups, which picture each group is drawn on and how the pictures are capped, how the
//!       spoken segments are cut by picture, the pasted text (the words with a mark such as
//!       `[画像 1 ①]` at the place each group was drawn, estimated from the spoken time inside
//!       the segment and moved to a boundary, with the picture path for a terminal), the
//!       bundle ids that count as a terminal, and every tuning constant.
//! Why: the pictures and the wording decide whether an agent understands the bundle. Keeping
//!      the rules pure (guardrail G1) makes them testable with numbers alone and lets the
//!      constants be tuned from real recordings without touching an adapter.
//! Where: `src-tauri/src/adapters/screen.rs` turns pictures into `ScreenSample`s (16 x 9 cell
//!        brightness), the drawing layer sends `InkStroke`s. While recording the adapter calls
//!        `ink_samples` to learn which pictures to keep. At stop the controller runs
//!        `select_keyframes` (`Effect::PlanKeyframes`), cuts the audio with
//!        `pipeline::segment_by_keyframes` at `keyframe_starts_in_samples` (a region before the
//!        first picture goes to the first picture: no word is lost), and renders the pasted text
//!        with `summary_text`. All times are milliseconds of recording time: the adapters remove
//!        paused time before the core sees it. Tests at the end of this file.

use crate::session::TranscriptText;
use crate::{Language, Millis, SAMPLE_RATE_HZ, pipeline};

// --- tuning constants (initial values; adjusted from real recordings) ---

/// The screen is divided into this grid; each cell carries its mean brightness (0 to 1).
pub const GRID_COLUMNS: usize = 16;
pub const GRID_ROWS: usize = 9;
pub const GRID_CELLS: usize = GRID_COLUMNS * GRID_ROWS;
/// A cell counts as changed between two samples when its brightness differs by more than this.
pub const CELL_CHANGE_THRESHOLD: f32 = 0.03;
/// Two pictures are the same screen when less than this share of cells changed: a new ink
/// group on the same screen is drawn on the previous picture, and strokes closer than
/// `INK_GROUP_GAP_MS` form one group only while the screen stays the same.
pub const SAME_SCREEN_CHANGE_MAX: f32 = 0.10;
/// Ink strokes closer than this in time form one group, unless the screen changed between them.
pub const INK_GROUP_GAP_MS: Millis = 1_500;
/// At most this many pictures per bundle; later ink groups are not used.
pub const MAX_KEYFRAMES: usize = 6;
/// A spoken segment belongs to the newest picture that starts before the segment start plus
/// this lead: people start talking a little before they draw.
pub const SEGMENT_LEAD_MS: Millis = 1_000;

/// Bundle ids of the apps that receive the text alone (the picture paths are inside the marks).
/// Every other app receives the pictures first, then the text. Herdr hosts terminals for coding
/// agents, so it counts as one.
pub const TERMINAL_BUNDLE_IDS: [&str; 8] = [
    "com.mitchellh.ghostty",
    "com.googlecode.iterm2",
    "com.apple.Terminal",
    "com.github.wez.wezterm",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "dev.warp.Warp-Stable",
    "dev.herdr.app",
];

// --- signals (recorded by the adapters, in recording time) ---

/// A position on the recorded display as a share of its width and height (0 to 1).
pub type Point = (f32, f32);

/// One picture of the screen, reduced to numbers. The picture itself stays in the adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenSample {
    pub t_ms: Millis,
    /// `GRID_CELLS` mean brightness values, row by row. Another length counts as a full change.
    pub cells: Vec<f32>,
}

/// One drawn line, from Shift-drag start to release.
#[derive(Debug, Clone, PartialEq)]
pub struct InkStroke {
    pub t0_ms: Millis,
    pub t1_ms: Millis,
    pub points: Vec<Point>,
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
    pub samples: Vec<ScreenSample>,
    pub strokes: Vec<InkStroke>,
    pub speech: Vec<SpeechSpan>,
}

// --- pictures and marks (the plan) ---

/// A chosen picture. `n` is its number, from 1 in time order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub n: usize,
    /// When its first ink group started drawing.
    pub t_ms: Millis,
    /// Index into `Signals::samples` of the picture: the one taken right before that.
    pub sample: usize,
}

/// One ink group drawn on a picture, with its circled number (`label`, from 1 in time order
/// across the bundle).
#[derive(Debug, Clone, PartialEq)]
pub struct Mark {
    pub keyframe: usize,
    pub label: u32,
    pub t0_ms: Millis,
    pub t1_ms: Millis,
    /// The strokes of the group, each a polyline.
    pub strokes: Vec<Vec<Point>>,
}

/// The result of `select_keyframes`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Selection {
    pub keyframes: Vec<Keyframe>,
    /// In time order, one per ink group that is drawn.
    pub marks: Vec<Mark>,
    /// Ink groups that are not drawn: over `MAX_KEYFRAMES`, or without any picture.
    pub dropped_groups: usize,
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

/// Consecutive strokes that belong together: the next starts within `INK_GROUP_GAP_MS` of the
/// previous end, and the screen did not change between them.
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
                && change_between(signals, g.t1_ms, stroke.t0_ms) < SAME_SCREEN_CHANGE_MAX
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
    let (Some(a), Some(b)) = (
        sample_before(&signals.samples, t0_ms),
        sample_before(&signals.samples, t1_ms),
    ) else {
        return 0.0;
    };
    match (signals.samples.get(a), signals.samples.get(b)) {
        (Some(a), Some(b)) => change(&a.cells, &b.cells),
        _ => 0.0,
    }
}

/// Index of the last sample taken before `t_ms` (the first sample when none is; `None` without
/// samples).
pub fn sample_before(samples: &[ScreenSample], t_ms: Millis) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    Some(samples.iter().rposition(|s| s.t_ms < t_ms).unwrap_or(0))
}

/// The samples to keep as pictures for the strokes so far: the one before each ink group's
/// first stroke. Pure, so the adapter may call it on every new stroke.
pub fn ink_samples(signals: &Signals) -> Vec<usize> {
    ink_groups(signals)
        .iter()
        .filter_map(|g| sample_before(&signals.samples, g.t0_ms))
        .collect()
}

/// Chooses the pictures and the marks.
///
/// 1. Each ink group is drawn on the picture taken right before its first stroke.
/// 2. When that picture shows the same screen as the previous picture (change below
///    `SAME_SCREEN_CHANGE_MAX`), the group is drawn on the previous picture instead.
/// 3. At most `MAX_KEYFRAMES` pictures; later groups are dropped and counted.
/// 4. Marks are numbered from 1 in time order. Without any sample nothing can be drawn.
pub fn select_keyframes(signals: &Signals) -> Selection {
    let mut selection = Selection::default();
    for group in ink_groups(signals) {
        let Some(sample) = sample_before(&signals.samples, group.t0_ms) else {
            selection.dropped_groups += 1;
            continue;
        };
        let same_as_previous = selection.keyframes.last().is_some_and(|k| {
            match (signals.samples.get(k.sample), signals.samples.get(sample)) {
                (Some(a), Some(b)) => change(&a.cells, &b.cells) < SAME_SCREEN_CHANGE_MAX,
                _ => false,
            }
        });
        let keyframe = match selection.keyframes.last() {
            Some(k) if same_as_previous => k.n,
            _ if selection.keyframes.len() >= MAX_KEYFRAMES => {
                selection.dropped_groups += 1;
                continue;
            }
            _ => {
                let n = selection.keyframes.len() + 1;
                selection.keyframes.push(Keyframe {
                    n,
                    t_ms: group.t0_ms,
                    sample,
                });
                n
            }
        };
        let strokes = signals.strokes[group.first..=group.last]
            .iter()
            .map(|s| s.points.clone())
            .collect();
        selection.marks.push(Mark {
            keyframe,
            label: selection.marks.len() as u32 + 1,
            t0_ms: group.t0_ms,
            t1_ms: group.t1_ms,
            strokes,
        });
    }
    selection
}

// --- audio segments and their picture ---

pub fn ms_to_samples(ms: Millis) -> usize {
    (ms as usize).saturating_mul(SAMPLE_RATE_HZ as usize) / 1_000
}

pub fn samples_to_ms(samples: usize) -> Millis {
    (samples as Millis).saturating_mul(1_000) / SAMPLE_RATE_HZ as Millis
}

/// The start of every picture in samples, ascending, for `pipeline::segment_by_keyframes`.
pub fn keyframe_starts_in_samples(keyframes: &[Keyframe]) -> Vec<usize> {
    keyframes.iter().map(|k| ms_to_samples(k.t_ms)).collect()
}

/// `SEGMENT_LEAD_MS` in samples, for `pipeline::segment_by_keyframes`.
pub fn segment_lead_samples() -> usize {
    ms_to_samples(SEGMENT_LEAD_MS)
}

/// The picture number that receives a segment starting at `start_ms`: the newest picture that
/// starts before `start_ms + SEGMENT_LEAD_MS`, or the first picture. `None` without pictures.
pub fn keyframe_for_segment(start_ms: Millis, keyframes: &[Keyframe]) -> Option<usize> {
    let limit = start_ms.saturating_add(SEGMENT_LEAD_MS);
    keyframes
        .iter()
        .rev()
        .find(|k| k.t_ms < limit)
        .or(keyframes.first())
        .map(|k| k.n)
}

// --- the pasted text ---

/// A transcribed segment, placed on a picture.
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
    /// Receives the text only; the marks carry the picture paths, spelled as a dragged file
    /// (`\ ` for a space).
    Terminal,
    /// Receives the pictures, then the text without paths.
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

/// The path as the text writes it for the target.
pub fn path_for_paste(path: &str, target: PasteTarget) -> String {
    match target {
        PasteTarget::Terminal => path.replace(' ', "\\ "),
        PasteTarget::Other => path.to_owned(),
    }
}

/// What the text is made of. Chunks and marks are in time order.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary<'a> {
    pub language: Language,
    pub target: PasteTarget,
    /// The absolute folder holding `frames/<n>.png`, plain. Only a terminal gets the paths.
    pub bundle_dir: &'a str,
    pub marks: &'a [Mark],
    pub chunks: &'a [Chunk],
    /// The spans in which speech was heard (VAD), to place a mark inside a chunk by the spoken
    /// time before it. Empty: the chunk's own span stands in.
    pub speech: &'a [SpeechSpan],
    /// The transcription failed: a note replaces the words; the marks still go out.
    pub transcription_failed: bool,
}

/// A mark is moved to the best boundary within this many characters of its estimated place.
pub const MARK_SNAP_CHARS: usize = 6;

/// Words a mark is placed in front of when they are near its estimated place.
pub const DEMONSTRATIVES: [&str; 14] = [
    "これ",
    "ここ",
    "この",
    "それ",
    "そこ",
    "その",
    "あれ",
    "あそこ",
    "あの",
    "こっち",
    "this",
    "that",
    "here",
    "there",
];

const INK_LABELS: [char; 20] = [
    '①', '②', '③', '④', '⑤', '⑥', '⑦', '⑧', '⑨', '⑩', '⑪', '⑫', '⑬', '⑭', '⑮', '⑯', '⑰', '⑱', '⑲',
    '⑳',
];

/// The circled number of a mark, as text.
pub fn ink_label(label: u32) -> String {
    match label
        .checked_sub(1)
        .and_then(|i| INK_LABELS.get(i as usize))
    {
        Some(c) => c.to_string(),
        None => format!("({label})"),
    }
}

fn image_word(language: Language) -> &'static str {
    match language {
        Language::Ja => "画像",
        Language::En => "image",
    }
}

fn failed_note(language: Language) -> &'static str {
    match language {
        Language::Ja => "(文字起こしに失敗しました)",
        Language::En => "(transcription failed)",
    }
}

/// The token of one mark: `[画像 1 ①]`; with the picture path (a terminal, the first mark of
/// the picture only).
fn mark_token(s: &Summary<'_>, mark: &Mark, with_path: bool) -> String {
    let mut token = format!(
        "[{} {} {}",
        image_word(s.language),
        mark.keyframe,
        ink_label(mark.label)
    );
    if with_path {
        let path = format!(
            "{}/frames/{}.png",
            s.bundle_dir.trim_end_matches('/'),
            mark.keyframe
        );
        token.push_str(": ");
        token.push_str(&path_for_paste(&path, s.target));
    }
    token.push(']');
    token
}

/// Share of the spoken time of a chunk that lies before `t_ms`: the VAD spans inside the
/// chunk, silence left out. Without a span the chunk's own span stands in.
pub fn spoken_share(chunk: &Chunk, speech: &[SpeechSpan], t_ms: Millis) -> f32 {
    let (cs, ce) = (chunk.start_ms, chunk.end_ms.max(chunk.start_ms));
    let mut total = 0u64;
    let mut before = 0u64;
    for span in speech {
        let (a, b) = (span.start_ms.max(cs), span.end_ms.min(ce));
        if b <= a {
            continue;
        }
        total += b - a;
        before += b.min(t_ms).saturating_sub(a);
    }
    if total == 0 {
        total = ce - cs;
        before = t_ms.min(ce).saturating_sub(cs);
    }
    if total == 0 {
        return 0.0;
    }
    (before as f32 / total as f32).clamp(0.0, 1.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Script {
    Kanji,
    Hiragana,
    Katakana,
    Latin,
    Other,
}

fn script_of(c: char) -> Script {
    match c {
        '\u{3040}'..='\u{309F}' => Script::Hiragana,
        '\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{FF66}'..='\u{FF9D}' => {
            Script::Katakana
        }
        _ if crate::text::is_cjk_char(c) => Script::Kanji,
        _ if c.is_alphanumeric() => Script::Latin,
        _ => Script::Other,
    }
}

fn is_punctuation(c: char) -> bool {
    matches!(
        c,
        '。' | '、' | '，' | '．' | '！' | '？' | '.' | ',' | '!' | '?' | ';' | ':'
    )
}

fn starts_with_demonstrative(chars: &[char], i: usize) -> bool {
    DEMONSTRATIVES.iter().any(|w| {
        let w: Vec<char> = w.chars().collect();
        chars.len() >= i + w.len()
            && chars[i..i + w.len()]
                .iter()
                .zip(&w)
                .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            && chars
                .get(i + w.len())
                .is_none_or(|c| !(w[0].is_ascii_alphabetic() && c.is_alphanumeric()))
    })
}

/// How good a place between `chars[i - 1]` and `chars[i]` is for a mark: 1 after a
/// punctuation mark, 2 before a demonstrative, 3 where the script changes (English: at a
/// space), `None` inside a word or a run of kanji.
fn boundary_rank(chars: &[char], i: usize, language: Language) -> Option<u8> {
    let n = chars.len();
    if i == 0 {
        return Some(3);
    }
    if i >= n {
        return Some(if chars.last().is_some_and(|c| is_punctuation(*c)) {
            1
        } else {
            3
        });
    }
    let (prev, next) = (chars[i - 1], chars[i]);
    match language {
        Language::En => {
            if !prev.is_whitespace() && !next.is_whitespace() {
                return None;
            }
            let before_word = prev.is_whitespace();
            let after_punct = i >= 2 && chars[i - 2..i].iter().any(|c| is_punctuation(*c));
            if before_word && after_punct {
                Some(1)
            } else if before_word && starts_with_demonstrative(chars, i) {
                Some(2)
            } else {
                Some(3)
            }
        }
        Language::Ja => {
            if is_punctuation(prev) {
                return Some(1);
            }
            if starts_with_demonstrative(chars, i) {
                return Some(2);
            }
            let (a, b) = (script_of(prev), script_of(next));
            if a == b && matches!(a, Script::Kanji | Script::Latin) {
                return None;
            }
            if a != b || a == Script::Other {
                Some(3)
            } else {
                // Inside a run of kana: allowed, but worse than a real boundary.
                Some(4)
            }
        }
    }
}

/// The character index of a mark in `text`: the estimated place (`share` of the characters),
/// moved to the best boundary within `MARK_SNAP_CHARS` (the best rank, then the nearest). With
/// no boundary near, the nearest boundary anywhere.
pub fn mark_index(text: &str, share: f32, language: Language) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let estimate = ((share * n as f32).round() as usize).min(n);
    let candidate =
        |i: usize| boundary_rank(&chars, i, language).map(|r| (r, i.abs_diff(estimate), i));
    let near = (estimate.saturating_sub(MARK_SNAP_CHARS)..=(estimate + MARK_SNAP_CHARS).min(n))
        .filter_map(candidate)
        .filter(|(r, _, _)| *r < 4)
        .min();
    if let Some((_, _, i)) = near {
        return i;
    }
    (0..=n)
        .filter_map(candidate)
        .map(|(r, d, i)| (d, r, i))
        .min()
        .map_or(estimate, |(_, _, i)| i)
}

/// Where each mark goes: the chunk whose span holds its start (by the spoken time before it),
/// the next chunk's head when it falls between chunks, the last chunk's end after the words.
/// Returns `(chunk index, character index)` per mark, in mark order; `None` without chunks.
fn mark_places(s: &Summary<'_>) -> Vec<Option<(usize, usize)>> {
    s.marks
        .iter()
        .map(|mark| {
            let t = mark.t0_ms;
            let inside = s
                .chunks
                .iter()
                .position(|c| c.start_ms <= t && t < c.end_ms);
            if let Some(i) = inside {
                let chunk = &s.chunks[i];
                let share = spoken_share(chunk, s.speech, t);
                return Some((i, mark_index(chunk.text.0.trim(), share, s.language)));
            }
            // Drawn before the words of the next chunk: its head. After all the words: the end.
            match s.chunks.iter().position(|c| c.start_ms > t) {
                Some(i) => Some((i, 0)),
                None => s
                    .chunks
                    .len()
                    .checked_sub(1)
                    .map(|i| (i, s.chunks[i].text.0.trim().chars().count())),
            }
        })
        .collect()
}

/// The text pasted after the pictures (and written to `summary.md`): the spoken words in
/// order, with the token of each ink group at the place it was drawn. A terminal
/// gets the picture path in the first token of each picture. Without ink: the words alone, as
/// a dictation. A failed transcription: a note, then the tokens.
pub fn summary_text(s: &Summary<'_>) -> String {
    let mut first_of_picture: Vec<usize> = Vec::new();
    let with_path = |mark: &Mark, seen: &mut Vec<usize>| -> bool {
        if s.target != PasteTarget::Terminal || seen.contains(&mark.keyframe) {
            return false;
        }
        seen.push(mark.keyframe);
        true
    };
    if s.transcription_failed {
        let mut parts = vec![failed_note(s.language).to_owned()];
        for mark in s.marks {
            let path = with_path(mark, &mut first_of_picture);
            parts.push(mark_token(s, mark, path));
        }
        return join_parts(&parts);
    }
    let places = mark_places(s);
    let mut parts: Vec<String> = Vec::new();
    for (ci, chunk) in s.chunks.iter().enumerate() {
        let chars: Vec<char> = chunk.text.0.trim().chars().collect();
        // Tokens of this chunk, by their character index, in mark order.
        let mut cut = 0;
        let mut piece = String::new();
        let flush_piece = |piece: &mut String, parts: &mut Vec<String>| {
            if !piece.trim().is_empty() {
                parts.push(piece.trim().to_owned());
            }
            piece.clear();
        };
        for (mark, place) in s.marks.iter().zip(&places) {
            let Some((chunk_index, at)) = place else {
                continue;
            };
            if *chunk_index != ci {
                continue;
            }
            let at = (*at).max(cut).min(chars.len());
            piece.extend(&chars[cut..at]);
            flush_piece(&mut piece, &mut parts);
            let path = with_path(mark, &mut first_of_picture);
            parts.push(mark_token(s, mark, path));
            cut = at;
        }
        piece.extend(&chars[cut..]);
        flush_piece(&mut piece, &mut parts);
    }
    if s.chunks.is_empty() {
        for mark in s.marks {
            let path = with_path(mark, &mut first_of_picture);
            parts.push(mark_token(s, mark, path));
        }
    }
    join_parts(&parts)
}

/// Words join as a dictation (`pipeline::join_texts`); a token always has a space on both sides.
fn join_parts(parts: &[String]) -> String {
    let mut out = String::new();
    for part in parts {
        let token = part.starts_with('[');
        if out.is_empty() {
            out.push_str(part);
        } else if token || out.ends_with(']') {
            out.push(' ');
            out.push_str(part);
        } else {
            out = pipeline::join_texts(&[out.clone(), part.clone()]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(level: f32) -> Vec<f32> {
        vec![level; GRID_CELLS]
    }

    /// A grid where `share` of the cells differ from `flat(0.2)` by far more than the threshold.
    fn grid(share: f32) -> Vec<f32> {
        let mut cells = flat(0.2);
        let n = (share * GRID_CELLS as f32).round() as usize;
        for c in cells.iter_mut().take(n) {
            *c = 0.9;
        }
        cells
    }

    fn sample(t_ms: Millis, cells: Vec<f32>) -> ScreenSample {
        ScreenSample { t_ms, cells }
    }

    /// Samples every 500 ms of the same screen.
    fn steady_samples(until_ms: Millis) -> Vec<ScreenSample> {
        (0..=until_ms / 500)
            .map(|i| sample(i * 500, flat(0.2)))
            .collect()
    }

    fn signals(until_ms: Millis) -> Signals {
        Signals {
            stopped_ms: until_ms,
            samples: steady_samples(until_ms),
            strokes: Vec::new(),
            speech: Vec::new(),
        }
    }

    fn stroke(t0_ms: Millis, t1_ms: Millis) -> InkStroke {
        InkStroke {
            t0_ms,
            t1_ms,
            points: vec![(0.1, 0.1), (0.2, 0.2)],
        }
    }

    fn span(start_ms: Millis, end_ms: Millis) -> SpeechSpan {
        SpeechSpan { start_ms, end_ms }
    }

    #[test]
    fn change_is_the_share_of_cells_over_the_threshold() {
        assert_eq!(change(&flat(0.2), &flat(0.2)), 0.0);
        assert!((change(&flat(0.2), &grid(0.5)) - 0.5).abs() < 0.01);
        // A difference under the threshold does not count.
        assert_eq!(change(&flat(0.2), &flat(0.22)), 0.0);
        // Different lengths are a full change.
        assert_eq!(change(&flat(0.2), &[0.2]), 1.0);
        assert_eq!(change(&[], &[]), 1.0);
    }

    #[test]
    fn strokes_group_within_the_gap_unless_the_screen_changes() {
        let mut s = signals(20_000);
        s.strokes = vec![
            stroke(2_000, 2_500),
            stroke(3_000, 3_400), // 500 ms after: same group
            stroke(6_000, 6_200), // 2.6 s after: a new group
        ];
        let groups = ink_groups(&s);
        assert_eq!(groups.len(), 2);
        assert_eq!((groups[0].first, groups[0].last), (0, 1));
        assert_eq!((groups[0].t0_ms, groups[0].t1_ms), (2_000, 3_400));
        assert_eq!((groups[1].first, groups[1].last), (2, 2));

        // The screen changes between two close strokes: separate groups.
        let mut s = signals(20_000);
        s.samples[6] = sample(3_000, grid(0.5));
        s.samples[7] = sample(3_500, grid(0.5));
        s.strokes = vec![stroke(2_000, 2_500), stroke(3_600, 3_800)];
        assert_eq!(ink_groups(&s).len(), 2);
    }

    #[test]
    fn no_samples_means_no_pictures_and_dropped_groups() {
        let s = Signals {
            stopped_ms: 5_000,
            strokes: vec![stroke(1_000, 1_500)],
            ..Signals::default()
        };
        let selection = select_keyframes(&s);
        assert!(selection.keyframes.is_empty());
        assert!(selection.marks.is_empty());
        assert_eq!(selection.dropped_groups, 1);
        assert!(ink_samples(&s).is_empty());
    }

    #[test]
    fn no_ink_means_no_pictures() {
        let s = signals(10_000);
        assert_eq!(select_keyframes(&s), Selection::default());
    }

    #[test]
    fn an_ink_group_is_drawn_on_the_picture_before_its_first_stroke() {
        let mut s = signals(10_000);
        s.strokes = vec![stroke(2_100, 2_600), stroke(2_900, 3_200)];
        let selection = select_keyframes(&s);
        assert_eq!(selection.keyframes.len(), 1);
        let k = &selection.keyframes[0];
        assert_eq!((k.n, k.t_ms), (1, 2_100));
        assert_eq!(s.samples[k.sample].t_ms, 2_000);
        assert_eq!(selection.marks.len(), 1);
        let m = &selection.marks[0];
        assert_eq!(
            (m.keyframe, m.label, m.t0_ms, m.t1_ms),
            (1, 1, 2_100, 3_200)
        );
        assert_eq!(m.strokes.len(), 2);
        assert_eq!(selection.dropped_groups, 0);
        assert_eq!(ink_samples(&s), vec![k.sample]);
    }

    #[test]
    fn a_group_on_the_same_screen_joins_the_previous_picture() {
        let mut s = signals(20_000);
        s.strokes = vec![stroke(2_000, 2_500), stroke(8_000, 8_500)];
        let selection = select_keyframes(&s);
        assert_eq!(selection.keyframes.len(), 1);
        assert_eq!(selection.marks.len(), 2);
        assert_eq!(
            selection
                .marks
                .iter()
                .map(|m| (m.keyframe, m.label))
                .collect::<Vec<_>>(),
            vec![(1, 1), (1, 2)]
        );
    }

    #[test]
    fn a_group_on_a_changed_screen_gets_its_own_picture() {
        let mut s = signals(20_000);
        for sample in s.samples.iter_mut().filter(|x| x.t_ms >= 5_000) {
            sample.cells = grid(0.5);
        }
        s.strokes = vec![stroke(2_000, 2_500), stroke(8_000, 8_500)];
        let selection = select_keyframes(&s);
        assert_eq!(selection.keyframes.len(), 2);
        assert_eq!(selection.keyframes[1].t_ms, 8_000);
        assert_eq!(s.samples[selection.keyframes[1].sample].t_ms, 7_500);
        assert_eq!(
            selection
                .marks
                .iter()
                .map(|m| (m.keyframe, m.label))
                .collect::<Vec<_>>(),
            vec![(1, 1), (2, 2)]
        );
    }

    #[test]
    fn more_than_six_pictures_drop_the_later_groups_with_their_marks() {
        let mut s = signals(60_000);
        // Every 5 s the screen changes; a stroke follows each change.
        for sample in s.samples.iter_mut() {
            let step = sample.t_ms / 5_000;
            sample.cells = if step % 2 == 0 { flat(0.2) } else { grid(0.5) };
        }
        s.strokes = (0..8)
            .map(|i| stroke(i * 5_000 + 1_000, i * 5_000 + 1_400))
            .collect();
        let selection = select_keyframes(&s);
        assert_eq!(selection.keyframes.len(), MAX_KEYFRAMES);
        // The seventh group shows a new screen and is dropped; the eighth shows the same screen
        // as the sixth picture and is drawn on it.
        assert_eq!(selection.marks.len(), MAX_KEYFRAMES + 1);
        assert_eq!(selection.dropped_groups, 1);
        assert_eq!(
            selection.marks.last().map(|m| (m.keyframe, m.label)),
            Some((6, 7))
        );
    }

    fn keyframes_at(times: &[Millis]) -> Vec<Keyframe> {
        times
            .iter()
            .enumerate()
            .map(|(i, t)| Keyframe {
                n: i + 1,
                t_ms: *t,
                sample: i,
            })
            .collect()
    }

    #[test]
    fn sample_conversions() {
        assert_eq!(ms_to_samples(1_000), SAMPLE_RATE_HZ as usize);
        assert_eq!(samples_to_ms(SAMPLE_RATE_HZ as usize * 3), 3_000);
        assert_eq!(ms_to_samples(0), 0);
    }

    #[test]
    fn keyframe_starts_feed_the_pipeline_with_the_same_rule() {
        let keyframes = keyframes_at(&[2_000, 9_000]);
        let starts = keyframe_starts_in_samples(&keyframes);
        assert_eq!(starts, vec![ms_to_samples(2_000), ms_to_samples(9_000)]);
        for start_ms in [0, 1_500, 2_000, 8_200, 9_500] {
            let ours = keyframe_for_segment(start_ms, &keyframes);
            let theirs =
                pipeline::keyframe_index(&starts, ms_to_samples(start_ms), segment_lead_samples())
                    + 1;
            assert_eq!(ours, Some(theirs), "at {start_ms} ms");
        }
    }

    #[test]
    fn words_before_the_first_picture_belong_to_it() {
        let keyframes = keyframes_at(&[10_000, 20_000]);
        assert_eq!(keyframe_for_segment(0, &keyframes), Some(1));
        assert_eq!(keyframe_for_segment(9_200, &keyframes), Some(1));
        assert_eq!(keyframe_for_segment(19_500, &keyframes), Some(2));
        assert_eq!(keyframe_for_segment(0, &[]), None);
    }

    #[test]
    fn terminal_bundle_ids() {
        assert!(is_terminal("com.mitchellh.ghostty"));
        assert!(is_terminal(" COM.APPLE.TERMINAL "));
        assert!(is_terminal("dev.herdr.app"));
        assert_eq!(paste_target(Some("dev.herdr.app")), PasteTarget::Terminal);
        assert!(!is_terminal("com.google.Chrome"));
        assert_eq!(
            paste_target(Some("com.googlecode.iterm2")),
            PasteTarget::Terminal
        );
        assert_eq!(paste_target(Some("com.apple.TextEdit")), PasteTarget::Other);
        assert_eq!(paste_target(None), PasteTarget::Other);
    }

    #[test]
    fn the_path_escapes_spaces_only_for_terminals() {
        let p = "/Users/me/Library/Application Support/x/frames/1.png";
        assert_eq!(
            path_for_paste(p, PasteTarget::Terminal),
            "/Users/me/Library/Application\\ Support/x/frames/1.png"
        );
        assert_eq!(path_for_paste(p, PasteTarget::Other), p);
    }

    fn chunk(start_ms: Millis, end_ms: Millis, keyframe: usize, text: &str) -> Chunk {
        Chunk {
            start_ms,
            end_ms,
            keyframe,
            text: TranscriptText(text.into()),
        }
    }

    fn mark(keyframe: usize, label: u32, t0_ms: Millis) -> Mark {
        Mark {
            keyframe,
            label,
            t0_ms,
            t1_ms: t0_ms + 600,
            strokes: vec![vec![(0.4, 0.2), (0.5, 0.3)]],
        }
    }

    const DIR: &str = "/Users/me/Library/Application Support/murmur/annotations/20261102-1200-ab12";

    fn summary<'a>(
        language: Language,
        target: PasteTarget,
        marks: &'a [Mark],
        chunks: &'a [Chunk],
        speech: &'a [SpeechSpan],
        failed: bool,
    ) -> Summary<'a> {
        Summary {
            language,
            target,
            bundle_dir: DIR,
            marks,
            chunks,
            speech,
            transcription_failed: failed,
        }
    }

    /// Spans that cover each chunk whole (no silence inside).
    fn continuous(chunks: &[Chunk]) -> Vec<SpeechSpan> {
        chunks.iter().map(|c| span(c.start_ms, c.end_ms)).collect()
    }

    #[test]
    fn spoken_share_leaves_silence_out() {
        let c = chunk(10_000, 20_000, 1, "x");
        // 10 s of chunk, but speech only in 10..12 and 18..20: 4 s spoken.
        let speech = [span(10_000, 12_000), span(18_000, 20_000)];
        assert_eq!(spoken_share(&c, &speech, 9_000), 0.0);
        assert_eq!(spoken_share(&c, &speech, 11_000), 0.25);
        assert_eq!(spoken_share(&c, &speech, 15_000), 0.5); // in the silence: half spoken
        assert_eq!(spoken_share(&c, &speech, 19_000), 0.75);
        assert_eq!(spoken_share(&c, &speech, 25_000), 1.0);
        // Without spans the chunk's own span stands in.
        assert_eq!(spoken_share(&c, &[], 15_000), 0.5);
        // Spans outside the chunk do not count.
        assert_eq!(spoken_share(&c, &[span(0, 5_000)], 15_000), 0.5);
    }

    #[test]
    fn mark_index_prefers_punctuation_then_demonstratives_then_script_changes() {
        // A punctuation mark within reach wins.
        let text = "ここを押す。そしてこれも押す";
        let i = mark_index(text, 0.45, Language::Ja); // estimate 6, 。 is at index 5
        assert_eq!(i, 6);
        // A demonstrative within reach, no punctuation near.
        let text = "ボタンだからこれを押してください";
        let i = mark_index(text, 0.5, Language::Ja); // estimate 8, これ starts at 6
        assert_eq!(i, 6);
        // A script change only: between kana and kanji.
        let text = "いいかんじだから採用します";
        let i = mark_index(text, 0.6, Language::Ja); // estimate 8: 採 is at 8
        assert_eq!(i, 8);
        // Never inside a run of kanji: 採用 stays whole.
        let i = mark_index("採用採用採用採用採用", 0.45, Language::Ja);
        assert!(i == 0 || i == 10);
        // The ends are always fine.
        assert_eq!(mark_index("言葉", 0.0, Language::Ja), 0);
        assert_eq!(mark_index("言葉", 1.0, Language::Ja), 2);
        assert_eq!(mark_index("", 0.5, Language::Ja), 0);
    }

    #[test]
    fn mark_index_in_english_lands_between_words() {
        let text = "press this button. then that one";
        // Estimate 14 (inside "button"): the punctuation boundary at 19 is within reach and
        // beats the plain word boundary at 11.
        assert_eq!(mark_index(text, 14.0 / 32.0, Language::En), 19);
        // Estimate 8 (inside "this"): no punctuation within reach; before "this" wins.
        assert_eq!(mark_index(text, 8.0 / 32.0, Language::En), 6);
        // Estimate 26 (inside "that"): before "that".
        assert_eq!(mark_index(text, 26.0 / 32.0, Language::En), 24);
        // Never inside a word.
        for i in 0..=32 {
            let at = mark_index(text, i as f32 / 32.0, Language::En);
            assert!(
                at == 0 || at == 32 || text.chars().nth(at - 1) == Some(' '),
                "{i} -> {at}"
            );
        }
    }

    #[test]
    fn the_second_mark_lands_before_the_second_demonstrative() {
        // Shaped like the real case: one chunk of 9.3 s, the first mark before the words, the
        // second about 63 % into them. Expected: ① at the head, ② after そして and before これも.
        let chunks = vec![chunk(
            11_000,
            20_300,
            1,
            "これはいい感じだから使おうかなそしてこれもいい感じだからこれも使おう。",
        )];
        let marks = vec![mark(1, 1, 8_800), mark(1, 2, 16_900)];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        assert_eq!(
            text,
            "[画像 1 ①] これはいい感じだから使おうかなそして [画像 1 ②] これもいい感じだからこれも使おう。"
        );
    }

    #[test]
    fn a_terminal_puts_the_path_in_the_first_token_of_each_picture_only() {
        let chunks = vec![chunk(
            11_000,
            20_300,
            1,
            "これはいい感じだから使おうかなそしてこれもいい感じだからこれも使おう。",
        )];
        let marks = vec![mark(1, 1, 8_800), mark(1, 2, 16_900), mark(2, 3, 25_000)];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Terminal,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        let path =
            "/Users/me/Library/Application\\ Support/murmur/annotations/20261102-1200-ab12/frames";
        assert_eq!(
            text,
            format!(
                "[画像 1 ①: {path}/1.png] これはいい感じだから使おうかなそして [画像 1 ②] これもいい感じだからこれも使おう。 [画像 2 ③: {path}/2.png]"
            )
        );
    }

    #[test]
    fn marks_between_chunks_go_to_the_head_of_the_next_chunk() {
        let chunks = vec![
            chunk(0, 3_000, 1, "このボタンを押すと、"),
            chunk(8_000, 12_000, 1, "ここの表示がずれるんですよ。"),
            chunk(21_000, 24_000, 2, "こっちは正しく出てます。"),
        ];
        let marks = vec![mark(1, 1, 5_000), mark(2, 2, 15_000)];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        assert_eq!(
            text,
            "このボタンを押すと、 [画像 1 ①] ここの表示がずれるんですよ。 [画像 2 ②] こっちは正しく出てます。"
        );
    }

    #[test]
    fn english_text_places_the_mark_at_a_word_boundary() {
        let chunks = vec![chunk(
            0,
            8_000,
            1,
            "When I press this button the layout breaks here.",
        )];
        let marks = vec![mark(1, 1, 5_000)];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::En,
            PasteTarget::Other,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        // 62 % of 48 characters is index 30, inside "layout": the boundary before it wins.
        assert_eq!(
            text,
            "When I press this button the [image 1 ①] layout breaks here."
        );
    }

    #[test]
    fn silence_inside_a_chunk_moves_the_mark_by_spoken_time() {
        // Words in 0..4 s and 10..14 s; the mark at 12 s is half into the second part, which
        // is 75 % of the spoken time: after the second sentence.
        let chunks = vec![chunk(
            0,
            14_000,
            1,
            "最初の文を話します。しばらく黙ります。それから次の文を話します。",
        )];
        let marks = vec![mark(1, 1, 12_000)];
        let speech = [span(0, 4_000), span(10_000, 14_000)];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        assert_eq!(
            text,
            "最初の文を話します。しばらく黙ります。 [画像 1 ①] それから次の文を話します。"
        );
        // Treated as continuous, 12 s of 14 s lands at the end instead.
        let spans = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            &spans,
            false,
        ));
        assert_eq!(
            text,
            "最初の文を話します。しばらく黙ります。それから次の文を話します。 [画像 1 ①]"
        );
    }

    #[test]
    fn marks_after_the_last_sentence_and_marks_without_words() {
        let chunks = vec![chunk(0, 2_000, 1, "見てください。")];
        let marks = vec![mark(1, 1, 5_000), mark(2, 2, 9_000)];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            &speech,
            false,
        ));
        assert_eq!(text, "見てください。 [画像 1 ①] [画像 2 ②]");
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &[],
            &[],
            false,
        ));
        assert_eq!(text, "[画像 1 ①] [画像 2 ②]");
    }

    #[test]
    fn without_ink_the_text_is_a_plain_dictation() {
        let chunks = vec![
            chunk(0, 2_000, 1, "公園に行きました。"),
            chunk(3_000, 5_000, 1, "コーヒーを飲みました。"),
        ];
        let speech = continuous(&chunks);
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Terminal,
            &[],
            &chunks,
            &speech,
            false,
        ));
        assert_eq!(text, "公園に行きました。コーヒーを飲みました。");
        let en = vec![
            chunk(0, 2_000, 1, "Open the settings."),
            chunk(3_000, 5_000, 1, "Press the hotkey."),
        ];
        let text = summary_text(&summary(
            Language::En,
            PasteTarget::Other,
            &[],
            &en,
            &[],
            false,
        ));
        assert_eq!(text, "Open the settings. Press the hotkey.");
    }

    #[test]
    fn failed_transcription_writes_a_note_then_the_marks() {
        let marks = vec![mark(1, 1, 2_000), mark(1, 2, 3_000)];
        let chunks = vec![chunk(0, 2_000, 1, "ignored")];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Terminal,
            &marks,
            &chunks,
            &[],
            true,
        ));
        assert_eq!(
            text,
            "(文字起こしに失敗しました) [画像 1 ①: /Users/me/Library/Application\\ Support/murmur/annotations/20261102-1200-ab12/frames/1.png] [画像 1 ②]"
        );
        let text = summary_text(&summary(
            Language::En,
            PasteTarget::Other,
            &[],
            &[],
            &[],
            true,
        ));
        assert_eq!(text, "(transcription failed)");
    }

    #[test]
    fn empty_chunks_are_skipped() {
        let chunks = vec![chunk(0, 1_000, 1, "  "), chunk(2_000, 3_000, 1, "言葉。")];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &[],
            &chunks,
            &[],
            false,
        ));
        assert_eq!(text, "言葉。");
    }

    #[test]
    fn ink_labels() {
        assert_eq!(ink_label(1), "①");
        assert_eq!(ink_label(20), "⑳");
        assert_eq!(ink_label(21), "(21)");
        assert_eq!(ink_label(0), "(0)");
    }

    #[test]
    fn speech_spans_do_not_move_pictures() {
        // The pictures depend on the strokes and the screen only.
        let mut s = signals(10_000);
        s.strokes = vec![stroke(4_000, 4_500)];
        let without = select_keyframes(&s);
        s.speech = vec![span(0, 9_000)];
        assert_eq!(select_keyframes(&s), without);
    }
}
