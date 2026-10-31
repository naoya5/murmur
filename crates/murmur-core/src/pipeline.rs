//! SOT: recognition-plan
//! What: how recorded audio is cut before it reaches the model: no speech, the whole buffer,
//!       or chunks along the silences that VAD found.
//! Why: Haas and Parakeet work on up to about 30 s at once; longer audio is split at silences
//!      (PLAN 2.7). Audio without speech never reaches the model, which removes most
//!      hallucinations.
//! Where: the engine adapter calls `plan` after `VoiceDetector::regions` and transcribes each
//!        range; tests at the end of this file.

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
    crate::text::is_cjk_char(c)
        || ('\u{3000}'..='\u{303F}').contains(&c)
        || ('\u{FF00}'..='\u{FFEF}').contains(&c)
}

fn ends_cjk(s: &str) -> bool {
    s.chars().next_back().is_some_and(is_cjk)
}

fn starts_cjk(s: &str) -> bool {
    s.chars().next().is_some_and(is_cjk)
}
