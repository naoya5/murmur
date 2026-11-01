//! SOT: recognition-plan
//! What: how recorded audio is cut before it reaches the model: no speech, the whole buffer,
//!       or chunks along the silences that VAD found.
//! Why: Haas and Parakeet work on up to about 30 s at once; longer audio is split at silences
//!      (PLAN 2.7). Audio without speech never reaches the model, which removes most
//!      hallucinations.
//! Where: the engine adapter calls `plan` after `VoiceDetector::regions` and transcribes each
//!        range. While recording (S1), `IncrementalPlan` yields the same chunks one at a time from
//!        the regions a streaming detector gives, so that only the tail is left at stop.
//!        Annotation sessions call `segment_at_boundaries` instead: the same regions, also cut
//!        where a keyframe starts, so that each segment's words belong to one picture
//!        (`annotations.rs` assigns them). Tests at the end of this file.

use std::collections::VecDeque;
use std::ops::Range;

use crate::SAMPLE_RATE_HZ;
use crate::ports::SpeechRegion;

/// Audio up to this length goes to the model in one piece.
pub const MAX_CHUNK_SAMPLES: usize = 30 * SAMPLE_RATE_HZ as usize;
/// Context kept around each chunk so that word edges are not clipped.
pub const CHUNK_PAD_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// VAD found no speech. Do not call the model.
    NoSpeech,
    /// Transcribe these sample ranges in order and join the texts.
    Chunks(Vec<Range<usize>>),
}

/// Decides what to transcribe.
///
/// - No regions: `NoSpeech`.
/// - Total length up to `MAX_CHUNK_SAMPLES`: one chunk over the whole buffer (the model sees the
///   full context, exactly as recorded).
/// - Longer: consecutive regions are packed greedily into chunks of at most `MAX_CHUNK_SAMPLES`,
///   each cut in the silence between regions, padded by `CHUNK_PAD_SAMPLES` and clamped to the
///   buffer. A single region longer than the limit is split into equal parts under the limit.
pub fn plan(total_samples: usize, regions: &[SpeechRegion]) -> Plan {
    let regions: Vec<SpeechRegion> = regions
        .iter()
        .copied()
        .filter(|r| r.end > r.start && r.start < total_samples)
        .map(|r| SpeechRegion {
            start: r.start,
            end: r.end.min(total_samples),
        })
        .collect();
    if regions.is_empty() {
        return Plan::NoSpeech;
    }
    if total_samples <= MAX_CHUNK_SAMPLES {
        return Plan::Chunks(std::iter::once(0..total_samples).collect());
    }

    let mut chunks: Vec<Range<usize>> = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for region in regions {
        for piece in split_long(region) {
            current = match current {
                None => Some(piece),
                Some(open)
                    if piece.end - open.start <= MAX_CHUNK_SAMPLES - 2 * CHUNK_PAD_SAMPLES =>
                {
                    Some(open.start..piece.end)
                }
                Some(open) => {
                    chunks.push(open);
                    Some(piece)
                }
            };
        }
    }
    if let Some(open) = current {
        chunks.push(open);
    }
    Plan::Chunks(
        chunks
            .into_iter()
            .map(|c| {
                c.start.saturating_sub(CHUNK_PAD_SAMPLES)
                    ..(c.end + CHUNK_PAD_SAMPLES).min(total_samples)
            })
            .collect(),
    )
}

fn split_long(region: SpeechRegion) -> Vec<Range<usize>> {
    let limit = MAX_CHUNK_SAMPLES - 2 * CHUNK_PAD_SAMPLES;
    let len = region.end - region.start;
    if len <= limit {
        return std::iter::once(region.start..region.end).collect();
    }
    let parts = len.div_ceil(limit);
    let size = len.div_ceil(parts);
    (0..parts)
        .map(|i| {
            let start = region.start + i * size;
            start..(start + size).min(region.end)
        })
        .collect()
}

/// The same chunks as `plan`, built one region at a time while the audio is still arriving.
///
/// Feed every region the moment the streaming detector makes it final (they come in order), then
/// ask `take_ready` for the chunks that can be transcribed with the audio heard so far. A chunk
/// closes only when a later region does not fit after it, exactly as in `plan`, so the open chunk
/// is never transcribed early. At stop, `finish` says whether the partial work counts at all:
/// a recording up to `MAX_CHUNK_SAMPLES` is transcribed whole, as today, and the partial chunks
/// are discarded.
///
/// Invariant (tested): `taken chunks ++ finish chunks == plan(total, regions)` for the same
/// regions, sample for sample.
#[derive(Debug, Clone, Default)]
pub struct IncrementalPlan {
    /// The chunk still growing, unpadded.
    open: Option<Range<usize>>,
    /// Closed, unpadded chunks not yet handed out (their padding may still be in the future).
    closed: VecDeque<Range<usize>>,
    /// Final regions seen, for the whole-buffer and no-speech decisions.
    regions: usize,
}

/// What `IncrementalPlan::finish` decides for the end of the recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    /// No region at all: do not call the model.
    NoSpeech,
    /// The recording fits in one piece: discard every partial chunk and transcribe `0..total`
    /// whole, exactly as `plan` does for a short recording.
    WholeBuffer,
    /// The chunks left after the ones `take_ready` gave, padded and clamped. May be empty.
    Tail(Vec<Range<usize>>),
}

impl IncrementalPlan {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a region that the detector made final. Regions arrive in order of their start.
    pub fn push(&mut self, region: SpeechRegion) {
        if region.end <= region.start {
            return;
        }
        self.regions += 1;
        for piece in split_long(region) {
            self.open = match self.open.take() {
                None => Some(piece),
                Some(open)
                    if piece.end - open.start <= MAX_CHUNK_SAMPLES - 2 * CHUNK_PAD_SAMPLES =>
                {
                    Some(open.start..piece.end)
                }
                Some(open) => {
                    self.closed.push_back(open);
                    Some(piece)
                }
            };
        }
    }

    /// The closed chunks whose padding lies within the `available` samples heard so far, padded.
    /// Transcribe them now; they will not change.
    pub fn take_ready(&mut self, available: usize) -> Vec<Range<usize>> {
        let mut ready = Vec::new();
        while let Some(chunk) = self.closed.front() {
            if chunk.end + CHUNK_PAD_SAMPLES > available {
                break;
            }
            if let Some(chunk) = self.closed.pop_front() {
                ready.push(pad(chunk, available));
            }
        }
        ready
    }

    /// Ends the recording at `total` samples. `tail` are the regions the detector gave at the end;
    /// the regions already pushed are kept.
    pub fn finish(mut self, total: usize, tail: &[SpeechRegion]) -> Finish {
        for region in tail {
            // Reapply the filter of `plan`: the end may overshoot the audio, as a detector pads
            // its last piece with silence.
            if region.start < total {
                self.push(SpeechRegion {
                    start: region.start,
                    end: region.end.min(total),
                });
            }
        }
        if self.regions == 0 {
            return Finish::NoSpeech;
        }
        if total <= MAX_CHUNK_SAMPLES {
            return Finish::WholeBuffer;
        }
        let mut chunks: Vec<Range<usize>> = self.closed.drain(..).map(|c| pad(c, total)).collect();
        if let Some(open) = self.open.take() {
            chunks.push(pad(open, total));
        }
        Finish::Tail(chunks)
    }
}

fn pad(chunk: Range<usize>, total: usize) -> Range<usize> {
    chunk.start.saturating_sub(CHUNK_PAD_SAMPLES)..(chunk.end + CHUNK_PAD_SAMPLES).min(total)
}

/// Joins chunk texts. Japanese text is joined without a space; other text with one space.
pub fn join_texts(parts: &[String]) -> String {
    let mut out = String::new();
    for part in parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
        if !out.is_empty() && !(ends_cjk(&out) || starts_cjk(part)) {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

fn is_cjk(c: char) -> bool {
    crate::text::is_cjk_char(c) || crate::text::is_cjk_punct(c)
}

fn ends_cjk(s: &str) -> bool {
    s.chars().next_back().is_some_and(is_cjk)
}

fn starts_cjk(s: &str) -> bool {
    s.chars().next().is_some_and(is_cjk)
}

// --- annotation segments ---

/// How far on each side of a keyframe boundary the quietest cut point is looked for (400 ms).
pub const BOUNDARY_SEARCH_SAMPLES: usize = SAMPLE_RATE_HZ as usize * 2 / 5;
/// Length of the quiet window whose centre becomes the cut point (20 ms).
pub const QUIET_WINDOW_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 50;
/// Hop between two quiet windows tried (5 ms).
const QUIET_HOP_SAMPLES: usize = QUIET_WINDOW_SAMPLES / 4;
/// A segment shorter than this (1 s of speech) joins a neighbour.
pub const MIN_SEGMENT_SAMPLES: usize = SAMPLE_RATE_HZ as usize;

/// One piece of an annotation recording to transcribe on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// What to give the engine: the speech plus context padding at the edges that VAD found.
    /// An edge cut inside speech (at a keyframe boundary) gets no padding, so that no word is
    /// heard twice.
    pub audio: Range<usize>,
    /// The speech itself, without padding. Its start decides which keyframe gets the words.
    pub speech: Range<usize>,
}

/// A speech range plus whether each edge came from a cut inside speech rather than from VAD.
#[derive(Debug, Clone)]
struct Piece {
    range: Range<usize>,
    cut_start: bool,
    cut_end: bool,
    /// Number of boundaries at or before the start; pieces in different slots never pack together.
    slot: usize,
}

/// Cuts the recording of an annotation session into segments.
///
/// - The speech regions from VAD are the base, cleaned as in `plan`. No region: no segments.
/// - A region that crosses a keyframe `boundary` (sample position) is cut at the centre of the
///   quietest `QUIET_WINDOW_SAMPLES` within `BOUNDARY_SEARCH_SAMPLES` of the boundary, inside
///   the region. `pcm` supplies the loudness; when it is too short the cut lands on the boundary.
/// - Consecutive pieces between two boundaries pack greedily into segments of at most
///   `MAX_CHUNK_SAMPLES`, as `plan` does; a single piece over the limit is split in equal parts.
/// - A segment with less than `MIN_SEGMENT_SAMPLES` of speech joins the previous segment, or the
///   next one when there is no previous or the previous is full, even across a boundary.
/// - VAD edges are padded by `CHUNK_PAD_SAMPLES` and clamped to the buffer; cut edges are not.
pub fn segment_at_boundaries(
    total_samples: usize,
    regions: &[SpeechRegion],
    boundaries: &[usize],
    pcm: &[f32],
) -> Vec<Segment> {
    let regions: Vec<SpeechRegion> = regions
        .iter()
        .copied()
        .filter(|r| r.end > r.start && r.start < total_samples)
        .map(|r| SpeechRegion {
            start: r.start,
            end: r.end.min(total_samples),
        })
        .collect();
    if regions.is_empty() {
        return Vec::new();
    }
    let mut boundaries: Vec<usize> = boundaries
        .iter()
        .copied()
        .filter(|b| *b > 0 && *b < total_samples)
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();

    let limit = MAX_CHUNK_SAMPLES - 2 * CHUNK_PAD_SAMPLES;
    let mut pieces: Vec<Piece> = Vec::new();
    for region in regions {
        let mut at = region.start;
        let mut cut_start = false;
        for &b in boundaries
            .iter()
            .filter(|b| region.start < **b && **b < region.end)
        {
            let cut = quietest_cut(pcm, b, at, region.end);
            if cut <= at || cut >= region.end {
                continue;
            }
            push_piece(&mut pieces, at..cut, cut_start, true, &boundaries, limit);
            at = cut;
            cut_start = true;
        }
        push_piece(
            &mut pieces,
            at..region.end,
            cut_start,
            false,
            &boundaries,
            limit,
        );
    }

    // Pack pieces of one slot greedily, as `plan` packs regions.
    let mut packed: Vec<Piece> = Vec::new();
    for piece in pieces {
        match packed.last_mut() {
            Some(open)
                if open.slot == piece.slot && piece.range.end - open.range.start <= limit =>
            {
                open.range.end = piece.range.end;
                open.cut_end = piece.cut_end;
            }
            _ => packed.push(piece),
        }
    }

    // Short segments join a neighbour (the previous when there is one).
    let mut merged: Vec<Piece> = Vec::new();
    let mut i = 0;
    while i < packed.len() {
        let piece = packed[i].clone();
        let short = piece.range.end - piece.range.start < MIN_SEGMENT_SAMPLES;
        if short {
            if let Some(prev) = merged.last_mut()
                && piece.range.end - prev.range.start <= limit
            {
                prev.range.end = piece.range.end;
                prev.cut_end = piece.cut_end;
                i += 1;
                continue;
            }
            if let Some(next) = packed.get(i + 1)
                && next.range.end - piece.range.start <= limit
            {
                merged.push(Piece {
                    range: piece.range.start..next.range.end,
                    cut_start: piece.cut_start,
                    cut_end: next.cut_end,
                    slot: piece.slot,
                });
                i += 2;
                continue;
            }
        }
        merged.push(piece);
        i += 1;
    }

    merged
        .into_iter()
        .map(|p| {
            let start = if p.cut_start {
                p.range.start
            } else {
                p.range.start.saturating_sub(CHUNK_PAD_SAMPLES)
            };
            let end = if p.cut_end {
                p.range.end
            } else {
                (p.range.end + CHUNK_PAD_SAMPLES).min(total_samples)
            };
            Segment {
                audio: start..end,
                speech: p.range,
            }
        })
        .collect()
}

fn push_piece(
    pieces: &mut Vec<Piece>,
    range: Range<usize>,
    cut_start: bool,
    cut_end: bool,
    boundaries: &[usize],
    limit: usize,
) {
    if range.end <= range.start {
        return;
    }
    let slot = boundaries.iter().filter(|b| **b <= range.start).count();
    let parts = split_long(SpeechRegion {
        start: range.start,
        end: range.end,
    });
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.into_iter().enumerate() {
        debug_assert!(part.end - part.start <= limit);
        pieces.push(Piece {
            range: part,
            cut_start: if i == 0 { cut_start } else { true },
            cut_end: if i == last { cut_end } else { true },
            slot,
        });
    }
}

/// The centre of the quietest window near `boundary`, kept inside `(lo, hi)`. Falls back to the
/// boundary itself when the audio is too short to judge.
fn quietest_cut(pcm: &[f32], boundary: usize, lo: usize, hi: usize) -> usize {
    let search_start = boundary.saturating_sub(BOUNDARY_SEARCH_SAMPLES).max(lo);
    let search_end = (boundary + BOUNDARY_SEARCH_SAMPLES).min(hi).min(pcm.len());
    if search_end <= search_start || search_end - search_start < QUIET_WINDOW_SAMPLES {
        return boundary.clamp(lo, hi);
    }
    let mut best_start = search_start;
    let mut best_energy = f32::INFINITY;
    let mut start = search_start;
    while start + QUIET_WINDOW_SAMPLES <= search_end {
        let energy = window_energy(pcm, start..start + QUIET_WINDOW_SAMPLES);
        if energy < best_energy {
            best_energy = energy;
            best_start = start;
        }
        start += QUIET_HOP_SAMPLES;
    }
    (best_start + QUIET_WINDOW_SAMPLES / 2).clamp(lo, hi)
}

/// Sum of squares over `range` (out-of-range samples count as silence).
pub fn window_energy(pcm: &[f32], range: Range<usize>) -> f32 {
    pcm.get(range)
        .map(|w| w.iter().map(|s| s * s).sum())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: usize = SAMPLE_RATE_HZ as usize;

    fn secs(s: f64) -> usize {
        (s * SR as f64) as usize
    }

    fn region(start_s: f64, end_s: f64) -> SpeechRegion {
        SpeechRegion {
            start: secs(start_s),
            end: secs(end_s),
        }
    }

    /// One chunk over the whole buffer.
    fn whole(total: usize) -> Plan {
        let range = 0..total;
        Plan::Chunks(vec![range])
    }

    fn chunks(plan: Plan) -> Vec<Range<usize>> {
        match plan {
            Plan::Chunks(c) => c,
            Plan::NoSpeech => panic!("expected chunks"),
        }
    }

    /// Every chunk is non-empty, inside the buffer, in order, and under the model limit.
    fn check_well_formed(chunks: &[Range<usize>], total: usize) {
        assert!(!chunks.is_empty());
        for c in chunks {
            assert!(c.start < c.end, "{c:?}");
            assert!(c.end <= total, "{c:?} exceeds {total}");
            assert!(c.end - c.start <= MAX_CHUNK_SAMPLES, "{c:?} over the limit");
        }
        for pair in chunks.windows(2) {
            assert!(pair[0].start < pair[1].start, "{pair:?} out of order");
        }
    }

    #[test]
    fn constants() {
        assert_eq!(MAX_CHUNK_SAMPLES, 480_000);
        assert_eq!(CHUNK_PAD_SAMPLES, 3_200);
    }

    #[test]
    fn no_regions_is_no_speech() {
        assert_eq!(plan(secs(5.0), &[]), Plan::NoSpeech);
        assert_eq!(plan(0, &[]), Plan::NoSpeech);
    }

    #[test]
    fn degenerate_regions_are_dropped() {
        // Empty, reversed, or entirely past the end of the buffer.
        let total = secs(5.0);
        let bad = [
            SpeechRegion { start: 10, end: 10 },
            SpeechRegion { start: 20, end: 10 },
            region(6.0, 7.0),
        ];
        assert_eq!(plan(total, &bad), Plan::NoSpeech);
        // One good region among them is enough.
        let mixed = [bad[0], region(1.0, 2.0), bad[2]];
        assert_eq!(plan(total, &mixed), whole(total));
    }

    #[test]
    fn short_buffer_is_one_chunk_over_everything() {
        let total = secs(12.0);
        let regions = [region(1.0, 3.0), region(5.0, 11.0)];
        assert_eq!(plan(total, &regions), whole(total));
        // Exactly the limit still goes in one piece.
        assert_eq!(
            plan(MAX_CHUNK_SAMPLES, &[region(0.0, 1.0)]),
            whole(MAX_CHUNK_SAMPLES)
        );
    }

    #[test]
    fn long_buffer_is_cut_in_the_silences() {
        let total = secs(70.0);
        let regions = [region(0.0, 20.0), region(25.0, 45.0), region(50.0, 70.0)];
        let c = chunks(plan(total, &regions));
        check_well_formed(&c, total);
        assert_eq!(
            c,
            vec![
                0..secs(20.0) + CHUNK_PAD_SAMPLES,
                secs(25.0) - CHUNK_PAD_SAMPLES..secs(45.0) + CHUNK_PAD_SAMPLES,
                secs(50.0) - CHUNK_PAD_SAMPLES..total,
            ]
        );
    }

    #[test]
    fn long_buffer_packs_neighbouring_regions_greedily() {
        let total = secs(60.0);
        // 10 s of speech, 2 s of silence, repeated: 0-10, 12-22, 24-34, 36-46, 48-58.
        let regions: Vec<SpeechRegion> = (0..5)
            .map(|i| region(i as f64 * 12.0, i as f64 * 12.0 + 10.0))
            .collect();
        let c = chunks(plan(total, &regions));
        check_well_formed(&c, total);
        // 0-22 (22 s) fits; adding 24-34 would make 34 s: cut. 24-46 fits; 24-58 does not.
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], 0..secs(22.0) + CHUNK_PAD_SAMPLES);
        assert_eq!(
            c[1],
            secs(24.0) - CHUNK_PAD_SAMPLES..secs(46.0) + CHUNK_PAD_SAMPLES
        );
        assert_eq!(
            c[2],
            secs(48.0) - CHUNK_PAD_SAMPLES..secs(58.0) + CHUNK_PAD_SAMPLES
        );
    }

    #[test]
    fn single_region_over_the_limit_is_split_in_equal_parts() {
        let total = secs(100.0);
        let c = chunks(plan(total, &[region(0.0, 100.0)]));
        check_well_formed(&c, total);
        // 100 s / 29.6 s -> 4 parts of 25 s each, padded.
        assert_eq!(c.len(), 4);
        assert_eq!(c[0], 0..secs(25.0) + CHUNK_PAD_SAMPLES);
        assert_eq!(
            c[1],
            secs(25.0) - CHUNK_PAD_SAMPLES..secs(50.0) + CHUNK_PAD_SAMPLES
        );
        assert_eq!(c[3], secs(75.0) - CHUNK_PAD_SAMPLES..total);
        // Together the chunks cover the region without a hole.
        for pair in c.windows(2) {
            assert!(pair[0].end >= pair[1].start);
        }
    }

    #[test]
    fn split_long_handles_a_region_just_over_the_limit() {
        let limit = MAX_CHUNK_SAMPLES - 2 * CHUNK_PAD_SAMPLES;
        let parts = split_long(SpeechRegion {
            start: 1_000,
            end: 1_000 + limit + 1,
        });
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].start, 1_000);
        assert_eq!(parts[1].end, 1_000 + limit + 1);
        assert!(parts.iter().all(|p| p.end - p.start <= limit));
        assert_eq!(parts[0].end, parts[1].start);

        let short = split_long(SpeechRegion { start: 5, end: 50 });
        assert_eq!(short, vec![5..50]);
    }

    #[test]
    fn padding_is_clamped_to_the_buffer() {
        let total = secs(40.0);
        // Speech starts at the very beginning and runs past the buffer end.
        let regions = [region(0.0, 10.0), region(30.0, 45.0)];
        let c = chunks(plan(total, &regions));
        check_well_formed(&c, total);
        assert_eq!(c.first().map(|c| c.start), Some(0));
        assert_eq!(c.last().map(|c| c.end), Some(total));
        // Pad does not reach before 0 for a region that starts inside the pad width.
        let c = chunks(plan(
            total,
            &[
                SpeechRegion {
                    start: 100,
                    end: secs(10.0),
                },
                region(30.0, 40.0),
            ],
        ));
        check_well_formed(&c, total);
        assert_eq!(c[0].start, 0);
    }

    #[test]
    fn join_japanese_without_spaces() {
        let parts = ["今日は、".to_owned(), "晴れです。".to_owned()];
        assert_eq!(join_texts(&parts), "今日は、晴れです。");
        let parts = ["「こんにちは」".to_owned(), "と言った".to_owned()];
        assert_eq!(join_texts(&parts), "「こんにちは」と言った");
    }

    #[test]
    fn join_english_with_one_space() {
        let parts = ["Hello,".to_owned(), "world.".to_owned(), "Bye.".to_owned()];
        assert_eq!(join_texts(&parts), "Hello, world. Bye.");
    }

    #[test]
    fn join_mixed_follows_the_boundary_characters() {
        let parts = [
            "Claude Code".to_owned(),
            "を使う。".to_owned(),
            "Fast.".to_owned(),
        ];
        assert_eq!(join_texts(&parts), "Claude Codeを使う。Fast.");
        let parts = ["今日は".to_owned(), "Claude Code".to_owned()];
        assert_eq!(join_texts(&parts), "今日はClaude Code");
    }

    #[test]
    fn join_skips_blank_parts_and_trims() {
        let parts = [
            "  Hello ".to_owned(),
            String::new(),
            "   ".to_owned(),
            " world ".to_owned(),
        ];
        assert_eq!(join_texts(&parts), "Hello world");
        assert_eq!(join_texts(&[]), "");
        assert_eq!(join_texts(&[String::new()]), "");
    }

    // --- IncrementalPlan ---

    /// xorshift64*, as in session.rs: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Random in-order regions: speech of 0.2 to 40 s separated by 0.1 to 8 s of silence.
    fn random_regions(rng: &mut Rng, total: usize) -> Vec<SpeechRegion> {
        let mut regions = Vec::new();
        let mut at = rng.below(secs(3.0) as u64) as usize;
        while at < total {
            let len = secs(0.2) + rng.below(secs(40.0) as u64) as usize;
            let end = (at + len).min(total + secs(0.3)); // the last may overshoot, as a VAD pads
            regions.push(SpeechRegion { start: at, end });
            at = end + secs(0.1) + rng.below(secs(8.0) as u64) as usize;
        }
        regions
    }

    /// Drives the plan the way the inference thread does: every region as it becomes final,
    /// `take_ready` after each with the audio heard so far, the last few at `finish`.
    fn stream(total: usize, regions: &[SpeechRegion], at_end: usize) -> Plan {
        let mut plan = IncrementalPlan::new();
        let mut taken = Vec::new();
        // A region is final while recording only once the silence that ends it was heard;
        // the rest (and the last `at_end` regions) arrive with `finish`.
        let closed_live = regions
            .iter()
            .take_while(|r| r.end + secs(0.5) <= total)
            .count();
        let live = closed_live.min(regions.len().saturating_sub(at_end));
        for region in &regions[..live] {
            plan.push(*region);
            // Heard so far: the region's end plus the silence that made it final.
            let available = (region.end + secs(0.5)).min(total);
            taken.extend(plan.take_ready(available));
        }
        match plan.finish(total, &regions[live..]) {
            Finish::NoSpeech => Plan::NoSpeech,
            Finish::WholeBuffer => Plan::Chunks(vec![Range {
                start: 0,
                end: total,
            }]),
            Finish::Tail(tail) => {
                taken.extend(tail);
                Plan::Chunks(taken)
            }
        }
    }

    #[test]
    fn incremental_plan_equals_plan_for_random_long_recordings() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let total = MAX_CHUNK_SAMPLES + 1 + rng.below(secs(600.0) as u64) as usize;
            let regions = random_regions(&mut rng, total);
            let expected = plan(total, &regions);
            for at_end in 0..=2.min(regions.len()) {
                let got = stream(total, &regions, at_end);
                assert_eq!(got, expected, "seed {seed}, {at_end} regions at the end");
            }
        }
    }

    #[test]
    fn incremental_plan_short_recording_is_the_whole_buffer() {
        let total = secs(20.0);
        let mut plan = IncrementalPlan::new();
        plan.push(region(1.0, 5.0));
        plan.push(region(8.0, 19.0));
        assert!(
            plan.take_ready(total).is_empty(),
            "nothing closes before an overflow"
        );
        assert_eq!(plan.finish(total, &[]), Finish::WholeBuffer);
        // Exactly the limit is still whole.
        let mut plan = IncrementalPlan::new();
        plan.push(region(0.0, 1.0));
        assert_eq!(plan.finish(MAX_CHUNK_SAMPLES, &[]), Finish::WholeBuffer);
    }

    #[test]
    fn incremental_plan_without_regions_is_no_speech() {
        let plan = IncrementalPlan::new();
        assert_eq!(plan.finish(secs(70.0), &[]), Finish::NoSpeech);
        let plan = IncrementalPlan::new();
        assert_eq!(
            plan.finish(secs(70.0), &[SpeechRegion { start: 5, end: 5 }]),
            Finish::NoSpeech
        );
        // A tail region starting past the end, as the offline filter drops it.
        let plan = IncrementalPlan::new();
        assert_eq!(
            plan.finish(secs(70.0), &[region(70.0, 71.0)]),
            Finish::NoSpeech
        );
    }

    #[test]
    fn incremental_plan_releases_a_chunk_only_when_its_padding_is_heard() {
        let mut plan = IncrementalPlan::new();
        plan.push(region(0.0, 20.0));
        // 25-45 does not fit after 0-20 (45 s > 29.6 s): 0-20 closes.
        plan.push(region(25.0, 45.0));
        assert!(
            plan.take_ready(secs(20.0)).is_empty(),
            "the 200 ms pad is not heard yet"
        );
        assert_eq!(
            plan.take_ready(secs(20.0) + CHUNK_PAD_SAMPLES),
            vec![0..secs(20.0) + CHUNK_PAD_SAMPLES]
        );
        assert!(
            plan.take_ready(secs(60.0)).is_empty(),
            "the open chunk never closes early"
        );
        let total = secs(70.0);
        assert_eq!(
            plan.finish(total, &[region(50.0, 70.0)]),
            Finish::Tail(vec![
                secs(25.0) - CHUNK_PAD_SAMPLES..secs(45.0) + CHUNK_PAD_SAMPLES,
                secs(50.0) - CHUNK_PAD_SAMPLES..total,
            ])
        );
    }

    #[test]
    fn incremental_plan_tail_overshoot_is_clamped_like_plan() {
        let total = secs(70.0);
        let regions = [region(0.0, 20.0), region(25.0, 45.0), region(50.0, 70.3)];
        assert_eq!(stream(total, &regions, 1), plan(total, &regions));
        assert_eq!(stream(total, &regions, 0), plan(total, &regions));
    }

    // --- segment_at_boundaries ---

    /// A loud tone with a 20 ms quiet dip at `dip_start` samples.
    fn pcm_with_dip(total: usize, dip_start: usize) -> Vec<f32> {
        (0..total)
            .map(|i| {
                if (dip_start..dip_start + QUIET_WINDOW_SAMPLES).contains(&i) {
                    0.001
                } else {
                    0.5
                }
            })
            .collect()
    }

    fn speech_of(segments: &[Segment]) -> Vec<Range<usize>> {
        segments.iter().map(|s| s.speech.clone()).collect()
    }

    /// Segments are ordered, non-empty, under the model limit, and inside the buffer; the audio
    /// contains the speech.
    fn check_segments(segments: &[Segment], total: usize) {
        for s in segments {
            assert!(s.speech.start < s.speech.end, "{s:?}");
            assert!(
                s.audio.start <= s.speech.start && s.speech.end <= s.audio.end,
                "{s:?}"
            );
            assert!(s.audio.end <= total, "{s:?} exceeds {total}");
            assert!(
                s.audio.end - s.audio.start <= MAX_CHUNK_SAMPLES,
                "{s:?} over the limit"
            );
        }
        for pair in segments.windows(2) {
            assert!(
                pair[0].speech.end <= pair[1].speech.start,
                "{pair:?} overlap"
            );
        }
    }

    #[test]
    fn segment_constants() {
        assert_eq!(BOUNDARY_SEARCH_SAMPLES, 6_400);
        assert_eq!(QUIET_WINDOW_SAMPLES, 320);
        assert_eq!(MIN_SEGMENT_SAMPLES, 16_000);
    }

    #[test]
    fn segments_without_regions_are_empty() {
        assert!(segment_at_boundaries(secs(10.0), &[], &[secs(5.0)], &[]).is_empty());
        let bad = [SpeechRegion { start: 20, end: 10 }, region(11.0, 12.0)];
        assert!(segment_at_boundaries(secs(10.0), &bad, &[], &[]).is_empty());
    }

    #[test]
    fn segments_without_boundaries_pack_the_regions_padded() {
        let total = secs(20.0);
        let regions = [region(1.0, 4.0), region(6.0, 9.0)];
        let got = segment_at_boundaries(total, &regions, &[], &[]);
        check_segments(&got, total);
        assert_eq!(
            got,
            vec![Segment {
                audio: secs(1.0) - CHUNK_PAD_SAMPLES..secs(9.0) + CHUNK_PAD_SAMPLES,
                speech: secs(1.0)..secs(9.0),
            }]
        );
        // Padding is clamped to the buffer.
        let got = segment_at_boundaries(secs(5.0), &[region(0.0, 5.5)], &[], &[]);
        assert_eq!(got[0].audio, 0..secs(5.0));
    }

    #[test]
    fn a_boundary_inside_speech_cuts_at_the_quietest_window() {
        let total = secs(20.0);
        let boundary = secs(10.0);
        // The dip sits 300 ms after the boundary, inside the 400 ms search range.
        let dip = boundary + secs(0.3);
        let pcm = pcm_with_dip(total, dip);
        let got = segment_at_boundaries(total, &[region(2.0, 18.0)], &[boundary], &pcm);
        check_segments(&got, total);
        assert_eq!(got.len(), 2);
        let cut = got[0].speech.end;
        assert_eq!(got[1].speech.start, cut);
        let centre = dip + QUIET_WINDOW_SAMPLES / 2;
        assert!(
            cut.abs_diff(centre) <= QUIET_WINDOW_SAMPLES / 4,
            "cut {cut} is not at the dip centre {centre}"
        );
        // No padding across the cut; padding at the VAD edges.
        assert_eq!(got[0].audio.end, cut);
        assert_eq!(got[1].audio.start, cut);
        assert_eq!(got[0].audio.start, secs(2.0) - CHUNK_PAD_SAMPLES);
        assert_eq!(got[1].audio.end, secs(18.0) + CHUNK_PAD_SAMPLES);
    }

    #[test]
    fn a_boundary_without_audio_cuts_at_the_boundary() {
        let total = secs(20.0);
        let got = segment_at_boundaries(total, &[region(2.0, 18.0)], &[secs(10.0)], &[]);
        assert_eq!(
            speech_of(&got),
            vec![secs(2.0)..secs(10.0), secs(10.0)..secs(18.0)]
        );
    }

    #[test]
    fn a_boundary_in_a_silence_keeps_the_regions_apart() {
        let total = secs(20.0);
        let regions = [region(1.0, 4.0), region(6.0, 9.0)];
        let got = segment_at_boundaries(total, &regions, &[secs(5.0)], &[]);
        check_segments(&got, total);
        assert_eq!(
            speech_of(&got),
            vec![secs(1.0)..secs(4.0), secs(6.0)..secs(9.0)]
        );
        // Both edges of each segment are VAD edges: padded.
        assert_eq!(
            got[0].audio,
            secs(1.0) - CHUNK_PAD_SAMPLES..secs(4.0) + CHUNK_PAD_SAMPLES
        );
        assert_eq!(
            got[1].audio,
            secs(6.0) - CHUNK_PAD_SAMPLES..secs(9.0) + CHUNK_PAD_SAMPLES
        );
    }

    #[test]
    fn boundaries_outside_speech_or_buffer_are_ignored() {
        let total = secs(20.0);
        let regions = [region(2.0, 8.0)];
        let got = segment_at_boundaries(
            total,
            &regions,
            &[0, secs(1.0), secs(9.0), total, total + 5],
            &[],
        );
        assert_eq!(speech_of(&got), vec![secs(2.0)..secs(8.0)]);
        // Several boundaries at the same place cut once.
        let got = segment_at_boundaries(total, &regions, &[secs(5.0), secs(5.0)], &[]);
        assert_eq!(
            speech_of(&got),
            vec![secs(2.0)..secs(5.0), secs(5.0)..secs(8.0)]
        );
    }

    #[test]
    fn a_short_piece_joins_the_previous_segment() {
        let total = secs(20.0);
        // The boundary leaves 0.5 s of speech after the cut.
        let got = segment_at_boundaries(total, &[region(2.0, 8.0)], &[secs(7.5)], &[]);
        assert_eq!(speech_of(&got), vec![secs(2.0)..secs(8.0)]);
        // The joined segment ends at a VAD edge again: padded.
        assert_eq!(got[0].audio.end, secs(8.0) + CHUNK_PAD_SAMPLES);
    }

    #[test]
    fn a_short_first_piece_joins_the_next_segment() {
        let total = secs(20.0);
        let got = segment_at_boundaries(total, &[region(2.0, 8.0)], &[secs(2.5)], &[]);
        assert_eq!(speech_of(&got), vec![secs(2.0)..secs(8.0)]);
        assert_eq!(got[0].audio.start, secs(2.0) - CHUNK_PAD_SAMPLES);
    }

    #[test]
    fn a_short_region_between_boundaries_joins_across_the_boundary() {
        let total = secs(30.0);
        let regions = [region(1.0, 5.0), region(10.0, 10.5), region(20.0, 25.0)];
        let got = segment_at_boundaries(total, &regions, &[secs(8.0), secs(15.0)], &[]);
        check_segments(&got, total);
        assert_eq!(
            speech_of(&got),
            vec![secs(1.0)..secs(10.5), secs(20.0)..secs(25.0)]
        );
    }

    #[test]
    fn long_speech_between_boundaries_is_split_under_the_limit() {
        let total = secs(100.0);
        let got = segment_at_boundaries(total, &[region(0.0, 100.0)], &[], &[]);
        check_segments(&got, total);
        assert_eq!(got.len(), 4);
        // Inner edges are cuts inside speech: no padding, no gap.
        for pair in got.windows(2) {
            assert_eq!(pair[0].audio.end, pair[1].audio.start);
            assert_eq!(pair[0].speech.end, pair[1].speech.start);
        }
        assert_eq!(got[0].audio.start, 0);
        assert_eq!(got[3].audio.end, total);
    }

    #[test]
    fn pieces_in_one_slot_pack_like_plan() {
        let total = secs(60.0);
        // 10 s of speech, 2 s of silence, repeated; one boundary at 30 s inside the silence.
        let regions: Vec<SpeechRegion> = (0..5)
            .map(|i| region(i as f64 * 12.0, i as f64 * 12.0 + 10.0))
            .collect();
        let got = segment_at_boundaries(total, &regions, &[secs(35.0)], &[]);
        check_segments(&got, total);
        // 0-22 fits, 24-34 does not join it; 24-34 is alone before the boundary; 36-58 after.
        assert_eq!(
            speech_of(&got),
            vec![
                0..secs(22.0),
                secs(24.0)..secs(34.0),
                secs(36.0)..secs(58.0)
            ]
        );
    }

    #[test]
    fn segments_cover_exactly_the_speech_for_random_inputs() {
        for seed in 1..=150u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let total = secs(5.0) + rng.below(secs(90.0) as u64) as usize;
            let regions = random_regions(&mut rng, total);
            let boundaries: Vec<usize> = (0..rng.below(8))
                .map(|_| rng.below(total as u64 + 10) as usize)
                .collect();
            let pcm: Vec<f32> = (0..total)
                .map(|i| ((i as f32) * 0.37).sin() * 0.5)
                .collect();
            let got = segment_at_boundaries(total, &regions, &boundaries, &pcm);
            check_segments(&got, total);
            // Every speech sample is in exactly one segment, and a segment starts and ends on
            // speech (it may span the silences between regions, as a chunk of `plan` does).
            let mut is_speech = vec![false; total];
            for r in &regions {
                for flag in &mut is_speech[r.start..r.end.min(total)] {
                    *flag = true;
                }
            }
            let mut covered = vec![false; total];
            for s in &got {
                assert!(
                    is_speech[s.speech.start],
                    "seed {seed}: {s:?} starts in silence"
                );
                assert!(
                    is_speech[s.speech.end - 1],
                    "seed {seed}: {s:?} ends in silence"
                );
                for i in s.speech.clone() {
                    assert!(!covered[i], "seed {seed}: sample {i} in two segments");
                    covered[i] = true;
                }
            }
            let missed = is_speech
                .iter()
                .zip(&covered)
                .filter(|(speech, c)| **speech && !**c)
                .count();
            assert_eq!(
                missed, 0,
                "seed {seed}: {missed} speech samples in no segment"
            );
        }
    }
}
