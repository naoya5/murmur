//! SOT: metrics
//! What: how words and characters are counted, and how the dashboard numbers are derived:
//!       time saved, speaking rate, latency median, streak, activity heatmap.
//! Why: Japanese has no word boundaries, so English counts words and Japanese counts characters
//!      (IDEATION 8). Saved time is derived when read, so a change of typing speed applies to the
//!      whole history at once.
//! Where: the store adapter calls `count` when it saves a transcript and keeps per-day totals;
//!        the `get_dashboard` command calls `dashboard`. Tests at the end of this file.

use serde::{Deserialize, Serialize};
use specta::Type;
use unicode_segmentation::UnicodeSegmentation;

use crate::Language;

/// Word and character counts of one text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Counts {
    /// Words that contain at least one Latin letter or digit (`unicode_words`, CJK excluded).
    pub latin_words: u32,
    /// Han, Hiragana and Katakana characters (`text::is_cjk_char`), no punctuation or space.
    pub cjk_chars: u32,
}

/// Counts a text. A mixed text has both counts.
pub fn count(text: &str) -> Counts {
    let cjk_chars = text
        .chars()
        .filter(|c| crate::text::is_cjk_char(*c))
        .count() as u32;
    let latin_words = text
        .unicode_words()
        .filter(|w| !w.chars().any(crate::text::is_cjk_char))
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .count() as u32;
    Counts {
        latin_words,
        cjk_chars,
    }
}

/// Typing speeds the user would otherwise type at (settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypingSpeeds {
    pub wpm_en: u32,
    pub cpm_ja: u32,
}

/// Estimated time to type the counts by hand, in ms.
pub fn typing_ms(counts: Counts, speeds: TypingSpeeds) -> u64 {
    let words = f64::from(counts.latin_words) / f64::from(speeds.wpm_en.max(1));
    let chars = f64::from(counts.cjk_chars) / f64::from(speeds.cpm_ja.max(1));
    ((words + chars) * 60_000.0).round() as u64
}

/// Typing time minus speaking time, never below zero.
pub fn saved_ms(counts: Counts, speech_ms: u64, speeds: TypingSpeeds) -> u64 {
    typing_ms(counts, speeds).saturating_sub(speech_ms)
}

/// Totals of one local day and one language, as the store keeps them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DayTotals {
    /// Days since 1970-01-01 in local time.
    pub day: i32,
    pub language: Language,
    pub sessions: u32,
    pub latin_words: u32,
    pub cjk_chars: u32,
    #[specta(type = f64)]
    pub speech_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct DayPoint {
    /// "YYYY-MM-DD".
    pub date: String,
    pub sessions: u32,
    pub latin_words: u32,
    pub cjk_chars: u32,
}

/// Everything the Home screen shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Dashboard {
    pub total_sessions: u32,
    pub total_latin_words: u32,
    pub total_cjk_chars: u32,
    #[specta(type = f64)]
    pub total_speech_ms: u64,
    #[specta(type = f64)]
    pub typing_ms: u64,
    #[specta(type = f64)]
    pub saved_ms: u64,
    /// English words per minute of English speech. `None` before any English session.
    pub speaking_wpm_en: Option<f64>,
    /// Japanese characters per minute of Japanese speech. `None` before any Japanese session.
    pub speaking_cpm_ja: Option<f64>,
    /// Median of the recent stop-to-clipboard latencies, ms.
    #[specta(type = Option<f64>)]
    pub median_latency_ms: Option<u64>,
    /// Consecutive days with at least one session, ending today (or yesterday if today has none).
    pub streak_days: u32,
    pub today_sessions: u32,
    /// The last 140 days (20 weeks), oldest first, for the heatmap.
    pub heatmap: Vec<DayPoint>,
    /// The last 14 days, oldest first, for the bar chart.
    pub last_14_days: Vec<DayPoint>,
}

/// Median of the values; the mean of the two middle values for an even count.
pub fn median(values: &[u64]) -> Option<u64> {
    // T2-CORE: implement; add tests.
    let _ = values;
    None
}

/// Converts days since 1970-01-01 to "YYYY-MM-DD" (proleptic Gregorian calendar).
pub fn day_to_iso(day: i32) -> String {
    // T2-CORE: implement with the days-from-civil algorithm (no chrono in core); add tests.
    day.to_string()
}

/// Builds the dashboard from per-day totals and recent latencies. `today` is days since epoch.
pub fn dashboard(
    totals: &[DayTotals],
    recent_latencies_ms: &[u64],
    today: i32,
    speeds: TypingSpeeds,
) -> Dashboard {
    // T2-CORE: implement as documented on `Dashboard`; add tests (streak gaps, mixed languages,
    // empty input, heatmap length 140 and last_14_days length 14 with zero days filled in).
    let _ = (totals, today, speeds);
    Dashboard {
        total_sessions: 0,
        total_latin_words: 0,
        total_cjk_chars: 0,
        total_speech_ms: 0,
        typing_ms: 0,
        saved_ms: 0,
        speaking_wpm_en: None,
        speaking_cpm_ja: None,
        median_latency_ms: median(recent_latencies_ms),
        streak_days: 0,
        today_sessions: 0,
        heatmap: Vec::new(),
        last_14_days: Vec::new(),
    }
}
