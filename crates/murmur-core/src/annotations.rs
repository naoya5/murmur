//! SOT: annotations
//! What: the rules of the experimental screen annotations. The signals the adapters record
//!       (screen samples as cell brightness, ink strokes, speech spans), how the strokes form
//!       groups, which picture each group is drawn on and how the pictures are capped, how the
//!       spoken segments are cut by picture, the pasted text (the words with a mark such as
//!       `[画像 1 ①]` before the sentence spoken while drawing, with the picture path for a
//!       terminal), the bundle ids that count as a terminal, and every tuning constant.
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
    /// The transcription failed: a note replaces the words; the marks still go out.
    pub transcription_failed: bool,
}

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

/// The mark of one picture: `[画像 1 ①②]`, with the picture path for a terminal.
fn mark_token(s: &Summary<'_>, keyframe: usize, labels: &[u32]) -> String {
    let labels: String = labels.iter().map(|l| ink_label(*l)).collect();
    let mut token = format!("[{} {keyframe} {labels}", image_word(s.language));
    if s.target == PasteTarget::Terminal {
        let path = format!(
            "{}/frames/{keyframe}.png",
            s.bundle_dir.trim_end_matches('/')
        );
        token.push_str(": ");
        token.push_str(&path_for_paste(&path, s.target));
    }
    token.push(']');
    token
}

/// The text pasted after the pictures (and written to `summary.md`): the spoken words in
/// order, with the mark of each ink group before the sentence spoken while it was drawn (the
/// chunk whose span holds the group's start, else the next chunk, else at the end). Marks of
/// one picture that fall at the same place share one token. Without ink: the words alone, as a
/// dictation. A failed transcription: a note, then the marks.
pub fn summary_text(s: &Summary<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut pending: Vec<&Mark> = s.marks.iter().collect();
    let mut flush = |parts: &mut Vec<String>, before_ms: Option<Millis>| {
        let take: Vec<&Mark> = pending
            .iter()
            .copied()
            .filter(|m| before_ms.is_none_or(|t| m.t0_ms < t))
            .collect();
        pending.retain(|m| before_ms.is_some_and(|t| m.t0_ms >= t));
        let mut i = 0;
        while i < take.len() {
            let keyframe = take[i].keyframe;
            let mut labels = Vec::new();
            while i < take.len() && take[i].keyframe == keyframe {
                labels.push(take[i].label);
                i += 1;
            }
            parts.push(mark_token(s, keyframe, &labels));
        }
    };
    if s.transcription_failed {
        parts.push(failed_note(s.language).to_owned());
        flush(&mut parts, None);
    } else {
        for chunk in s.chunks {
            flush(&mut parts, Some(chunk.end_ms));
            let text = chunk.text.0.trim();
            if !text.is_empty() {
                parts.push(text.to_owned());
            }
        }
        flush(&mut parts, None);
    }
    join_parts(&parts)
}

/// Words join as a dictation (`pipeline::join_texts`); a mark always has a space on both sides.
fn join_parts(parts: &[String]) -> String {
    let mut out = String::new();
    for part in parts {
        let mark = part.starts_with('[');
        if out.is_empty() {
            out.push_str(part);
        } else if mark || out.ends_with(']') {
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
        failed: bool,
    ) -> Summary<'a> {
        Summary {
            language,
            target,
            bundle_dir: DIR,
            marks,
            chunks,
            transcription_failed: failed,
        }
    }

    #[test]
    fn japanese_text_puts_each_mark_before_the_sentence_spoken_while_drawing() {
        let chunks = vec![
            chunk(0, 3_000, 1, "このボタンを押すと、"),
            chunk(8_000, 12_000, 1, "ここの表示がずれるんですよ。"),
            chunk(21_000, 24_000, 2, "こっちは正しく出てます。"),
        ];
        let marks = vec![mark(1, 1, 9_000), mark(2, 2, 21_500)];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            false,
        ));
        assert_eq!(
            text,
            "このボタンを押すと、 [画像 1 ①] ここの表示がずれるんですよ。 [画像 2 ②] こっちは正しく出てます。"
        );
    }

    #[test]
    fn a_terminal_gets_the_picture_path_in_the_mark() {
        let chunks = vec![chunk(0, 3_000, 1, "ここです。")];
        let marks = vec![mark(1, 1, 1_000)];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Terminal,
            &marks,
            &chunks,
            false,
        ));
        assert_eq!(
            text,
            "[画像 1 ①: /Users/me/Library/Application\\ Support/murmur/annotations/20261102-1200-ab12/frames/1.png] ここです。"
        );
    }

    #[test]
    fn english_text_uses_image_and_spaces() {
        let chunks = vec![
            chunk(0, 3_000, 1, "When I press this button,"),
            chunk(4_000, 7_000, 1, "the layout breaks here."),
        ];
        let marks = vec![mark(1, 1, 4_500)];
        let text = summary_text(&summary(
            Language::En,
            PasteTarget::Other,
            &marks,
            &chunks,
            false,
        ));
        assert_eq!(
            text,
            "When I press this button, [image 1 ①] the layout breaks here."
        );
    }

    #[test]
    fn marks_of_one_picture_at_the_same_place_share_a_token() {
        let chunks = vec![
            chunk(0, 2_000, 1, "まず、"),
            chunk(10_000, 12_000, 1, "両方です。"),
        ];
        let marks = vec![mark(1, 1, 3_000), mark(1, 2, 5_000)];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            false,
        ));
        assert_eq!(text, "まず、 [画像 1 ①②] 両方です。");
    }

    #[test]
    fn marks_after_the_last_sentence_and_marks_without_words() {
        let chunks = vec![chunk(0, 2_000, 1, "見てください。")];
        let marks = vec![mark(1, 1, 5_000), mark(2, 2, 9_000)];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            false,
        ));
        assert_eq!(text, "見てください。 [画像 1 ①] [画像 2 ②]");
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
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
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Terminal,
            &[],
            &chunks,
            false,
        ));
        assert_eq!(text, "公園に行きました。コーヒーを飲みました。");
        let en = vec![
            chunk(0, 2_000, 1, "Open the settings."),
            chunk(3_000, 5_000, 1, "Press the hotkey."),
        ];
        let text = summary_text(&summary(Language::En, PasteTarget::Other, &[], &en, false));
        assert_eq!(text, "Open the settings. Press the hotkey.");
    }

    #[test]
    fn failed_transcription_writes_a_note_then_the_marks() {
        let marks = vec![mark(1, 1, 2_000)];
        let chunks = vec![chunk(0, 2_000, 1, "ignored")];
        let text = summary_text(&summary(
            Language::Ja,
            PasteTarget::Other,
            &marks,
            &chunks,
            true,
        ));
        assert_eq!(text, "(文字起こしに失敗しました) [画像 1 ①]");
        let text = summary_text(&summary(Language::En, PasteTarget::Other, &[], &[], true));
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
