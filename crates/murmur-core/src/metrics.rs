//! SOT: metrics
//! What: how words and characters are counted, and how the dashboard numbers are derived:
//!       time saved, speaking rate, latency median, streak, activity heatmap.
//! Why: Japanese has no word boundaries, so English counts words and Japanese counts characters
//!      (IDEATION 8). Saved time is derived when read, so a change of typing speed applies to the
//!      whole history at once.
//! Where: the store adapter calls `count` when it saves a transcript and keeps per-day totals;
//!        the `get_dashboard` command calls `dashboard`. Tests at the end of this file.

use std::collections::BTreeMap;

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

/// Days shown by the heatmap (20 weeks).
pub const HEATMAP_DAYS: i32 = 140;
/// Days shown by the bar chart.
pub const BAR_CHART_DAYS: i32 = 14;

/// Median of the values; the mean of the two middle values (rounded down) for an even count.
pub fn median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return sorted.get(mid).copied();
    }
    match (sorted.get(mid.wrapping_sub(1)), sorted.get(mid)) {
        // Overflow-free mean of two u64 values.
        (Some(&a), Some(&b)) => Some(a / 2 + b / 2 + (a % 2 + b % 2) / 2),
        _ => None,
    }
}

/// Converts days since 1970-01-01 to "YYYY-MM-DD" (proleptic Gregorian calendar).
pub fn day_to_iso(day: i32) -> String {
    let (y, m, d) = civil_from_days(i64::from(day));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parses "YYYY-MM-DD" into days since 1970-01-01. The inverse of `day_to_iso`.
pub fn iso_to_day(iso: &str) -> Option<i32> {
    let mut parts = iso.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let day = days_from_civil(y, m, d);
    // Reject dates such as 2023-02-30 that the arithmetic silently rolls over.
    if civil_from_days(day) != (y, m, d) {
        return None;
    }
    i32::try_from(day).ok()
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) to days since 1970-01-01. Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = u64::from(if m > 2 { m - 3 } else { m + 9 }); // [0, 11]
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe as i64 - 719_468
}

/// Per-day sums over every language.
#[derive(Debug, Clone, Copy, Default)]
struct DaySum {
    sessions: u32,
    latin_words: u32,
    cjk_chars: u32,
}

/// Builds the dashboard from per-day totals and recent latencies. `today` is days since epoch.
///
/// - Totals and typing time sum every row, whatever its language.
/// - Saved time is computed from the totals (not summed per session), so a very fast speaker
///   on one day does not hide a slow one on another.
/// - Speaking rates use the unit of the language: English words over English speech time,
///   Japanese characters over Japanese speech time. `None` while that speech time is zero.
/// - The streak ends today, or yesterday when today has no session yet.
pub fn dashboard(
    totals: &[DayTotals],
    recent_latencies_ms: &[u64],
    today: i32,
    speeds: TypingSpeeds,
) -> Dashboard {
    let mut by_day: BTreeMap<i32, DaySum> = BTreeMap::new();
    let mut sum = DaySum::default();
    let mut total_speech_ms: u64 = 0;
    let mut en_words: u64 = 0;
    let mut en_ms: u64 = 0;
    let mut ja_chars: u64 = 0;
    let mut ja_ms: u64 = 0;

    for t in totals {
        let day = by_day.entry(t.day).or_default();
        day.sessions = day.sessions.saturating_add(t.sessions);
        day.latin_words = day.latin_words.saturating_add(t.latin_words);
        day.cjk_chars = day.cjk_chars.saturating_add(t.cjk_chars);
        sum.sessions = sum.sessions.saturating_add(t.sessions);
        sum.latin_words = sum.latin_words.saturating_add(t.latin_words);
        sum.cjk_chars = sum.cjk_chars.saturating_add(t.cjk_chars);
        total_speech_ms = total_speech_ms.saturating_add(t.speech_ms);
        match t.language {
            Language::En => {
                en_words += u64::from(t.latin_words);
                en_ms = en_ms.saturating_add(t.speech_ms);
            }
            Language::Ja => {
                ja_chars += u64::from(t.cjk_chars);
                ja_ms = ja_ms.saturating_add(t.speech_ms);
            }
        }
    }

    let per_minute = |amount: u64, ms: u64| -> Option<f64> {
        (ms > 0).then(|| amount as f64 / (ms as f64 / 60_000.0))
    };
    let sessions_on = |day: i32| by_day.get(&day).map_or(0, |d| d.sessions);
    let point = |day: i32| {
        let d = by_day.get(&day).copied().unwrap_or_default();
        DayPoint {
            date: day_to_iso(day),
            sessions: d.sessions,
            latin_words: d.latin_words,
            cjk_chars: d.cjk_chars,
        }
    };
    let window = |days: i32| -> Vec<DayPoint> {
        (0..days)
            .rev()
            .map(|back| point(today.saturating_sub(back)))
            .collect()
    };

    let mut streak_days: u32 = 0;
    let mut day = if sessions_on(today) > 0 {
        today
    } else {
        today.saturating_sub(1)
    };
    while sessions_on(day) > 0 && day > i32::MIN {
        streak_days = streak_days.saturating_add(1);
        day -= 1;
    }

    let counts = Counts {
        latin_words: sum.latin_words,
        cjk_chars: sum.cjk_chars,
    };
    Dashboard {
        total_sessions: sum.sessions,
        total_latin_words: sum.latin_words,
        total_cjk_chars: sum.cjk_chars,
        total_speech_ms,
        typing_ms: typing_ms(counts, speeds),
        saved_ms: saved_ms(counts, total_speech_ms, speeds),
        speaking_wpm_en: per_minute(en_words, en_ms),
        speaking_cpm_ja: per_minute(ja_chars, ja_ms),
        median_latency_ms: median(recent_latencies_ms),
        streak_days,
        today_sessions: sessions_on(today),
        heatmap: window(HEATMAP_DAYS),
        last_14_days: window(BAR_CHART_DAYS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEEDS: TypingSpeeds = TypingSpeeds {
        wpm_en: 40,
        cpm_ja: 80,
    };

    fn counts(latin_words: u32, cjk_chars: u32) -> Counts {
        Counts {
            latin_words,
            cjk_chars,
        }
    }

    fn row(
        day: i32,
        language: Language,
        sessions: u32,
        words: u32,
        chars: u32,
        ms: u64,
    ) -> DayTotals {
        DayTotals {
            day,
            language,
            sessions,
            latin_words: words,
            cjk_chars: chars,
            speech_ms: ms,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    // --- count ---

    #[test]
    fn count_english() {
        assert_eq!(count("Hello, world!"), counts(2, 0));
        assert_eq!(count("I opened the pull request."), counts(5, 0));
        assert_eq!(count("don't stop-believing"), counts(3, 0));
    }

    #[test]
    fn count_japanese() {
        assert_eq!(count("今日は晴れです。"), counts(0, 7));
        assert_eq!(count("カタカナとひらがなと漢字、々。"), counts(0, 13));
        assert_eq!(count("　「　」、。！？"), counts(0, 0));
    }

    #[test]
    fn count_mixed() {
        assert_eq!(count("今日は Claude Code で書きました。"), counts(2, 9));
        assert_eq!(count("Murmur は速い"), counts(1, 3));
    }

    #[test]
    fn count_numbers_and_punctuation() {
        assert_eq!(count("3 apples and 4.5 pears"), counts(5, 0));
        assert_eq!(count("... --- !!!"), counts(0, 0));
        assert_eq!(count(""), counts(0, 0));
        assert_eq!(count("会議は10時から"), counts(1, 6));
    }

    // --- typing and saved time ---

    #[test]
    fn typing_time_from_both_units() {
        // 40 words at 40 wpm = 1 min; 80 chars at 80 cpm = 1 min.
        assert_eq!(typing_ms(counts(40, 0), SPEEDS), 60_000);
        assert_eq!(typing_ms(counts(0, 80), SPEEDS), 60_000);
        assert_eq!(typing_ms(counts(40, 80), SPEEDS), 120_000);
        assert_eq!(typing_ms(counts(20, 0), SPEEDS), 30_000);
        assert_eq!(typing_ms(counts(0, 0), SPEEDS), 0);
    }

    #[test]
    fn typing_time_survives_zero_speed() {
        let zero = TypingSpeeds {
            wpm_en: 0,
            cpm_ja: 0,
        };
        assert_eq!(typing_ms(counts(1, 1), zero), 120_000);
    }

    #[test]
    fn saved_time_never_negative() {
        assert_eq!(saved_ms(counts(40, 0), 20_000, SPEEDS), 40_000);
        assert_eq!(saved_ms(counts(40, 0), 60_000, SPEEDS), 0);
        assert_eq!(saved_ms(counts(40, 0), 90_000, SPEEDS), 0);
    }

    // --- median ---

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[7]), Some(7));
        assert_eq!(median(&[3, 1, 2]), Some(2));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2));
        assert_eq!(median(&[1, 2, 3, 4, 5, 6]), Some(3));
        assert_eq!(median(&[10, 10]), Some(10));
        assert_eq!(median(&[u64::MAX, u64::MAX]), Some(u64::MAX));
        assert_eq!(median(&[5, 5, 5, 100]), Some(5));
    }

    // --- dates ---

    #[test]
    fn day_to_iso_known_dates() {
        assert_eq!(day_to_iso(0), "1970-01-01");
        assert_eq!(day_to_iso(1), "1970-01-02");
        assert_eq!(day_to_iso(-1), "1969-12-31");
        assert_eq!(day_to_iso(-365), "1969-01-01");
        assert_eq!(day_to_iso(10_957), "2000-01-01");
        assert_eq!(day_to_iso(11_016), "2000-02-29");
        assert_eq!(day_to_iso(19_723), "2024-01-01");
        assert_eq!(day_to_iso(19_782), "2024-02-29");
        assert_eq!(day_to_iso(19_783), "2024-03-01");
        assert_eq!(day_to_iso(20_758), "2026-11-01");
        assert_eq!(day_to_iso(2_932_896), "9999-12-31");
    }

    #[test]
    fn iso_round_trip_over_many_days() {
        for day in (-200_000..200_000).step_by(37) {
            let iso = day_to_iso(day);
            assert_eq!(iso_to_day(&iso), Some(day), "{iso}");
        }
    }

    #[test]
    fn iso_rejects_invalid_dates() {
        for bad in [
            "",
            "2024",
            "2024-13-01",
            "2024-00-10",
            "2024-02-30",
            "2023-02-29",
            "x-1-1",
            "2024-1-1-1",
        ] {
            assert_eq!(iso_to_day(bad), None, "{bad}");
        }
        assert_eq!(iso_to_day("2024-02-29"), Some(19_782));
    }

    // --- dashboard ---

    const TODAY: i32 = 20_758; // 2026-11-01

    #[test]
    fn dashboard_empty() {
        let d = dashboard(&[], &[], TODAY, SPEEDS);
        assert_eq!(d.total_sessions, 0);
        assert_eq!(d.total_latin_words, 0);
        assert_eq!(d.total_cjk_chars, 0);
        assert_eq!(d.total_speech_ms, 0);
        assert_eq!(d.typing_ms, 0);
        assert_eq!(d.saved_ms, 0);
        assert_eq!(d.speaking_wpm_en, None);
        assert_eq!(d.speaking_cpm_ja, None);
        assert_eq!(d.median_latency_ms, None);
        assert_eq!(d.streak_days, 0);
        assert_eq!(d.today_sessions, 0);
        assert_eq!(d.heatmap.len(), 140);
        assert_eq!(d.last_14_days.len(), 14);
        assert!(
            d.heatmap
                .iter()
                .all(|p| p.sessions == 0 && p.latin_words == 0 && p.cjk_chars == 0)
        );
    }

    #[test]
    fn dashboard_totals_mixed_languages() {
        let totals = [
            row(TODAY, Language::En, 2, 80, 0, 60_000),
            row(TODAY, Language::Ja, 1, 2, 160, 60_000),
            row(TODAY - 1, Language::En, 1, 40, 0, 30_000),
        ];
        let d = dashboard(&totals, &[300, 100, 200], TODAY, SPEEDS);
        assert_eq!(d.total_sessions, 4);
        assert_eq!(d.total_latin_words, 122);
        assert_eq!(d.total_cjk_chars, 160);
        assert_eq!(d.total_speech_ms, 150_000);
        // 122 words / 40 wpm = 3.05 min; 160 chars / 80 cpm = 2 min.
        assert_eq!(d.typing_ms, 303_000);
        assert_eq!(d.saved_ms, 153_000);
        assert_eq!(d.median_latency_ms, Some(200));
        assert_eq!(d.today_sessions, 3);
        assert_eq!(d.streak_days, 2);
    }

    #[test]
    fn speaking_rates_per_language() {
        let totals = [
            row(TODAY, Language::En, 1, 150, 0, 60_000),
            row(TODAY, Language::En, 1, 150, 0, 60_000),
            row(TODAY - 3, Language::Ja, 1, 5, 400, 120_000),
        ];
        let d = dashboard(&totals, &[], TODAY, SPEEDS);
        assert!(
            d.speaking_wpm_en.is_some_and(|v| close(v, 150.0)),
            "{:?}",
            d.speaking_wpm_en
        );
        assert!(
            d.speaking_cpm_ja.is_some_and(|v| close(v, 200.0)),
            "{:?}",
            d.speaking_cpm_ja
        );
    }

    #[test]
    fn speaking_rate_none_without_that_language_or_time() {
        let en_only = [row(TODAY, Language::En, 1, 100, 0, 30_000)];
        let d = dashboard(&en_only, &[], TODAY, SPEEDS);
        assert!(d.speaking_wpm_en.is_some());
        assert_eq!(d.speaking_cpm_ja, None);

        let zero_time = [row(TODAY, Language::Ja, 1, 0, 100, 0)];
        let d = dashboard(&zero_time, &[], TODAY, SPEEDS);
        assert_eq!(d.speaking_cpm_ja, None);
        assert_eq!(d.speaking_wpm_en, None);
    }

    #[test]
    fn streak_counts_from_today() {
        let totals: Vec<DayTotals> = (0..5)
            .map(|back| row(TODAY - back, Language::Ja, 1, 0, 10, 1_000))
            .collect();
        assert_eq!(dashboard(&totals, &[], TODAY, SPEEDS).streak_days, 5);
    }

    #[test]
    fn streak_counts_from_yesterday_when_today_is_empty() {
        let totals: Vec<DayTotals> = (1..4)
            .map(|back| row(TODAY - back, Language::En, 1, 10, 0, 1_000))
            .collect();
        let d = dashboard(&totals, &[], TODAY, SPEEDS);
        assert_eq!(d.streak_days, 3);
        assert_eq!(d.today_sessions, 0);
    }

    #[test]
    fn streak_stops_at_a_gap() {
        let totals = [
            row(TODAY, Language::En, 1, 10, 0, 1_000),
            row(TODAY - 1, Language::En, 1, 10, 0, 1_000),
            // TODAY - 2 missing
            row(TODAY - 3, Language::En, 1, 10, 0, 1_000),
            row(TODAY - 4, Language::En, 1, 10, 0, 1_000),
        ];
        assert_eq!(dashboard(&totals, &[], TODAY, SPEEDS).streak_days, 2);
    }

    #[test]
    fn streak_zero_when_today_and_yesterday_are_empty() {
        let totals = [row(TODAY - 2, Language::En, 3, 10, 0, 1_000)];
        assert_eq!(dashboard(&totals, &[], TODAY, SPEEDS).streak_days, 0);
    }

    #[test]
    fn streak_ignores_days_with_zero_sessions() {
        let totals = [
            row(TODAY, Language::En, 1, 10, 0, 1_000),
            row(TODAY - 1, Language::En, 0, 0, 0, 0),
            row(TODAY - 2, Language::En, 1, 10, 0, 1_000),
        ];
        assert_eq!(dashboard(&totals, &[], TODAY, SPEEDS).streak_days, 1);
    }

    #[test]
    fn heatmap_and_bars_are_zero_filled_and_ordered() {
        let totals = [
            row(TODAY, Language::En, 2, 20, 0, 1_000),
            row(TODAY, Language::Ja, 1, 0, 30, 1_000),
            row(TODAY - 13, Language::Ja, 4, 0, 40, 1_000),
            row(TODAY - 139, Language::En, 5, 50, 0, 1_000),
            row(TODAY - 140, Language::En, 9, 90, 0, 1_000), // outside the heatmap
            row(TODAY + 1, Language::En, 9, 90, 0, 1_000),   // a future day is not shown
        ];
        let d = dashboard(&totals, &[], TODAY, SPEEDS);

        assert_eq!(d.heatmap.len(), 140);
        assert_eq!(d.heatmap[0].date, day_to_iso(TODAY - 139));
        assert_eq!(d.heatmap[0].sessions, 5);
        assert_eq!(d.heatmap[139].date, "2026-11-01");
        assert_eq!(d.heatmap[139].sessions, 3);
        assert_eq!(d.heatmap[139].latin_words, 20);
        assert_eq!(d.heatmap[139].cjk_chars, 30);
        assert_eq!(d.heatmap.iter().filter(|p| p.sessions > 0).count(), 3);
        for pair in d.heatmap.windows(2) {
            assert!(pair[0].date < pair[1].date);
        }

        assert_eq!(d.last_14_days.len(), 14);
        assert_eq!(d.last_14_days[0].date, day_to_iso(TODAY - 13));
        assert_eq!(d.last_14_days[0].sessions, 4);
        assert_eq!(d.last_14_days[0].cjk_chars, 40);
        assert_eq!(d.last_14_days[13], d.heatmap[139]);
        assert_eq!(d.last_14_days.iter().filter(|p| p.sessions > 0).count(), 2);

        // Totals still include every row, shown or not.
        assert_eq!(d.total_sessions, 30);
    }
}
