//! SOT: text-rules
//! What: the rule-based formatting applied to model output: filler removal, known hallucination
//!       phrases, duplicate words, capitalization, CJK spacing, final punctuation, and dictionary
//!       replacements for languages whose model has no custom vocabulary.
//! Why: the models already punctuate; these cheap rules (< 1 ms, budget 2 ms) fix the rest
//!      without the latency of an LLM (IDEATION 9).
//! Where: the controller calls `format` between the engine and the clipboard. Tests at the end.

use crate::Language;
use crate::settings::DictionaryEntry;

/// Rule switches, read from settings at the moment of formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOptions {
    pub remove_fillers: bool,
    pub add_final_punctuation: bool,
    /// Apply dictionary entries as text replacements (true when the model has no custom
    /// vocabulary for this language, for example English on Parakeet).
    pub apply_dictionary: bool,
}

/// English fillers, removed as whole words, case-insensitive, with an adjacent comma.
pub const FILLERS_EN: &[&str] = &["um", "umm", "uh", "uhh", "erm", "er", "hmm", "mm"];

/// Japanese fillers, removed when they stand alone between punctuation or at the text edges.
pub const FILLERS_JA: &[&str] = &[
    "えー",
    "えーと",
    "えっと",
    "えーっと",
    "あのー",
    "あの〜",
    "うーん",
    "んー",
    "まー",
];

/// Output that models produce for silence or noise. A result equal to one of these (after
/// trimming punctuation and space, case-insensitive) is treated as no speech.
pub const HALLUCINATIONS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you",
    "you",
    "[blank_audio]",
    "[music]",
    "(music)",
    "ご視聴ありがとうございました",
    "ご清聴ありがとうございました",
    "字幕作成",
    "チャンネル登録よろしくお願いします",
];

/// Han, Hiragana or Katakana (the characters counted as Japanese).
pub fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'   // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{31F0}'..='\u{31FF}' // Katakana phonetic extensions
        | '\u{FF66}'..='\u{FF9D}' // Halfwidth Katakana
        | '\u{4E00}'..='\u{9FFF}' // CJK unified ideographs
        | '\u{3400}'..='\u{4DBF}' // Extension A
        | '\u{F900}'..='\u{FAFF}' // Compatibility ideographs
        | '\u{20000}'..='\u{2FA1F}' // Extensions B and later
        | '\u{3005}' | '\u{3007}' // 々 〇
    )
}

/// True when the whole text is a known hallucination phrase (see `HALLUCINATIONS`).
pub fn is_hallucination(text: &str) -> bool {
    // T2-CORE: implement as documented; add tests.
    let _ = text;
    false
}

/// Formats raw model output. Pure and deterministic.
///
/// Order: trim -> dictionary replacements (if `apply_dictionary`) -> fillers (if
/// `remove_fillers`) -> duplicate English words ("the the") -> whitespace (collapse runs; remove
/// spaces between two CJK characters and between CJK and CJK punctuation) -> capitalize the first
/// letter of each English sentence -> final punctuation (if `add_final_punctuation`: "." for
/// English, "。" for Japanese, unless the text already ends with terminal punctuation or a
/// closing bracket/quote after one).
///
/// Dictionary replacement: for each entry in scope for `lang`, each reading (and the written form
/// itself) is matched case-insensitively and, for Latin text, only at word boundaries; matches are
/// replaced by `written`. Longer readings are replaced first.
pub fn format(
    raw: &str,
    lang: Language,
    opts: &FormatOptions,
    dictionary: &[DictionaryEntry],
) -> String {
    // T2-CORE: implement as documented; add tests for English, Japanese and mixed text.
    let _ = (lang, opts, dictionary);
    raw.trim().to_owned()
}
