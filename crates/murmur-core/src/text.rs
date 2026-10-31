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

/// CJK punctuation and symbols (U+3000-303F) and full-width forms (U+FF00-FFEF), for example
/// 、 。 「 」 ！ ？ and the ideographic space. Japanese letters in those blocks are excluded.
pub fn is_cjk_punct(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{303F}' | '\u{FF00}'..='\u{FFEF}') && !is_cjk_char(c)
}

/// A letter or digit of a Latin-style word (anything alphanumeric that is not Japanese).
fn is_latin_word_char(c: char) -> bool {
    c.is_alphanumeric() && !is_cjk_char(c)
}

/// Punctuation that ends a sentence or a clause. Nothing is appended after one of these.
fn is_sentence_end(c: char) -> bool {
    matches!(
        c,
        '.' | '!'
            | '?'
            | ','
            | ';'
            | ':'
            | '\u{2026}' // …
            | '\u{2025}' // ‥
            | '。'
            | '、'
            | '！'
            | '？'
            | '，'
            | '．'
            | '；'
            | '：'
    )
}

/// A closing quote or bracket that may follow the final punctuation.
fn is_closing(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | ')'
            | ']'
            | '}'
            | '\u{201D}' // ”
            | '\u{2019}' // ’
            | '」'
            | '』'
            | '）'
            | '】'
            | '〕'
            | '》'
            | '〉'
    )
}

/// An opening quote or bracket that may precede the first letter of a sentence.
fn is_opening(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | '('
            | '['
            | '{'
            | '\u{201C}' // “
            | '\u{2018}' // ‘
    )
}

/// The comparable core of a phrase: lower-case ASCII, no surrounding space or punctuation,
/// inner whitespace collapsed. Brackets are kept so that "[blank_audio]" stays distinct.
fn phrase_core(text: &str) -> String {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || is_sentence_end(c));
    let mut out = String::with_capacity(trimmed.len());
    for word in trimmed.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().map(|c| c.to_ascii_lowercase()));
    }
    out
}

/// True when the whole text is a known hallucination phrase (see `HALLUCINATIONS`).
/// An empty text is not a hallucination; the caller reports it as an empty result.
pub fn is_hallucination(text: &str) -> bool {
    let core = phrase_core(text);
    !core.is_empty() && HALLUCINATIONS.iter().any(|h| phrase_core(h) == core)
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
///
/// Details that the rules above leave open:
/// - Case-insensitive means ASCII case only. Kana has no case and this keeps the scan allocation
///   free.
/// - Fillers of both languages are removed from any text, because one text can mix languages.
///   An English filler written in capitals ("ER", "MM") or after a number ("5 mm") is kept.
/// - Duplicate removal keeps the first word and the punctuation of the last one ("the the." ->
///   "the."). It also removes legitimate repeats such as "had had"; IDEATION 9 accepts that.
/// - Capitalization applies to English text only, after ". ! ?" followed by a space, and leaves
///   words that already contain a capital ("iPhone") alone.
/// - The spaces that Haas writes between Latin and Japanese ("Claude Code で") are kept.
pub fn format(
    raw: &str,
    lang: Language,
    opts: &FormatOptions,
    dictionary: &[DictionaryEntry],
) -> String {
    let mut chars: Vec<char> = raw.trim().chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if opts.apply_dictionary && !dictionary.is_empty() {
        chars = apply_dictionary(&chars, lang, dictionary);
    }
    if opts.remove_fillers {
        chars = remove_fillers_en(&chars);
        chars = remove_fillers_ja(&chars);
    }
    chars = remove_duplicate_words(&chars);
    chars = normalize_whitespace(&chars);
    if lang == Language::En {
        capitalize_sentences(&mut chars);
    }
    if opts.add_final_punctuation {
        add_final_punctuation(&mut chars, lang);
    }
    chars.into_iter().collect()
}

fn apply_dictionary(chars: &[char], lang: Language, dictionary: &[DictionaryEntry]) -> Vec<char> {
    // (what the model wrote, what to write instead), longest first.
    let mut patterns: Vec<(Vec<char>, &str)> = Vec::new();
    for entry in dictionary.iter().filter(|e| e.scope.includes(lang)) {
        let written = entry.written.trim();
        if written.is_empty() {
            continue;
        }
        for reading in entry
            .readings
            .iter()
            .map(|r| r.trim())
            .chain(std::iter::once(written))
            .filter(|r| !r.is_empty())
        {
            patterns.push((reading.chars().collect(), written));
        }
    }
    if patterns.is_empty() {
        return chars.to_vec();
    }
    patterns.sort_by_key(|(pattern, _)| std::cmp::Reverse(pattern.len()));

    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match patterns
            .iter()
            .find(|(pattern, _)| matches_at(chars, i, pattern))
        {
            Some((pattern, written)) => {
                out.extend(written.chars());
                i += pattern.len();
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Case-insensitive match of `pattern` at `i`, at word boundaries where the pattern edge is a
/// Latin word character.
fn matches_at(chars: &[char], i: usize, pattern: &[char]) -> bool {
    let (Some(&first), Some(&last)) = (pattern.first(), pattern.last()) else {
        return false;
    };
    let end = i + pattern.len();
    if end > chars.len() {
        return false;
    }
    if !chars[i..end]
        .iter()
        .zip(pattern)
        .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        return false;
    }
    if is_latin_word_char(first)
        && i > 0
        && chars.get(i - 1).is_some_and(|c| is_latin_word_char(*c))
    {
        return false;
    }
    if is_latin_word_char(last) && chars.get(end).is_some_and(|c| is_latin_word_char(*c)) {
        return false;
    }
    true
}

/// End of the Latin word that starts at `start`.
fn word_end(chars: &[char], start: usize) -> usize {
    let mut end = start;
    while chars.get(end).is_some_and(|c| is_latin_word_char(*c)) {
        end += 1;
    }
    end
}

fn is_filler_en(word: &[char]) -> bool {
    FILLERS_EN.iter().any(|f| {
        f.len() == word.len() && f.chars().zip(word).all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// True when the last non-space character before `i` is a digit ("5 mm" is a length).
fn preceded_by_digit(chars: &[char], i: usize) -> bool {
    chars[..i]
        .iter()
        .rev()
        .find(|c| !c.is_whitespace())
        .is_some_and(|c| c.is_ascii_digit())
}

/// Removes trailing whitespace from `out`, then one trailing comma if `comma` matches it.
fn pop_trailing_comma(out: &mut Vec<char>, comma: fn(char) -> bool) {
    while out.last().is_some_and(|c| c.is_whitespace()) {
        out.pop();
    }
    if out.last().is_some_and(|c| comma(*c)) {
        out.pop();
    }
}

/// Index just past a character matching `punct` that follows `from` (spaces allowed between).
fn punct_after(chars: &[char], from: usize, punct: fn(char) -> bool) -> Option<usize> {
    let mut j = from;
    while chars.get(j).is_some_and(|c| c.is_whitespace()) {
        j += 1;
    }
    chars.get(j).filter(|c| punct(**c)).map(|_| j + 1)
}

/// A filler occupying `..end` was dropped. Removes one adjacent comma (the following one first)
/// and returns where scanning continues. A filler that opens the text also takes its sentence
/// punctuation with it ("Hmm. I see." -> "I see.").
fn after_filler(chars: &[char], end: usize, out: &mut Vec<char>, comma: fn(char) -> bool) -> usize {
    if let Some(next) = punct_after(chars, end, comma) {
        return next;
    }
    if out.iter().all(|c| c.is_whitespace())
        && let Some(next) = punct_after(chars, end, is_sentence_end)
    {
        out.clear();
        return next;
    }
    pop_trailing_comma(out, comma);
    end
}

fn is_ascii_comma(c: char) -> bool {
    c == ','
}

fn is_ja_comma(c: char) -> bool {
    matches!(c, '、' | '，' | ',')
}

fn remove_fillers_en(chars: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let at_word_start =
            c.is_ascii_alphabetic() && (i == 0 || !is_latin_word_char(chars[i - 1]));
        if at_word_start {
            let end = word_end(chars, i);
            let word = &chars[i..end];
            let acronym = word.len() >= 2 && word.iter().all(|c| c.is_ascii_uppercase());
            if is_filler_en(word) && !acronym && !preceded_by_digit(chars, i) {
                i = after_filler(chars, end, &mut out, is_ascii_comma);
                continue;
            }
            out.extend_from_slice(word);
            i = end;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Where a Japanese filler may start or end: the text edge, punctuation or a space.
fn is_ja_boundary(c: char) -> bool {
    c.is_whitespace() || is_cjk_punct(c) || matches!(c, ',' | '.' | '!' | '?')
}

fn remove_fillers_ja(chars: &[char]) -> Vec<char> {
    let mut fillers: Vec<Vec<char>> = FILLERS_JA.iter().map(|f| f.chars().collect()).collect();
    fillers.sort_by_key(|f| std::cmp::Reverse(f.len()));

    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let at_boundary = i == 0 || is_ja_boundary(chars[i - 1]);
        if at_boundary {
            let hit = fillers.iter().find(|f| {
                chars[i..].starts_with(f)
                    && chars
                        .get(i + f.len())
                        .is_none_or(|next| is_ja_boundary(*next))
            });
            if let Some(f) = hit {
                i = after_filler(chars, i + f.len(), &mut out, is_ja_comma);
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Collapses "the the" (ASCII letters only, case-insensitive) into the first word.
fn remove_duplicate_words(chars: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let at_word_start =
            c.is_ascii_alphabetic() && (i == 0 || !is_latin_word_char(chars[i - 1]));
        if at_word_start {
            let end = word_end(chars, i);
            let word = &chars[i..end];
            out.extend_from_slice(word);
            i = end;
            if !word.iter().all(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            // Skip every following copy of the same word.
            loop {
                let mut k = i;
                while chars.get(k).is_some_and(|c| c.is_whitespace()) {
                    k += 1;
                }
                if k == i {
                    break;
                }
                let k_end = word_end(chars, k);
                let next = &chars[k..k_end];
                let same = next.len() == word.len()
                    && next
                        .iter()
                        .zip(word)
                        .all(|(a, b)| a.eq_ignore_ascii_case(b));
                if !same {
                    break;
                }
                i = k_end;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn is_cjk_like(c: char) -> bool {
    is_cjk_char(c) || is_cjk_punct(c)
}

/// Collapses whitespace runs to one space, drops spaces inside Japanese text and before
/// punctuation, and trims both ends.
fn normalize_whitespace(chars: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut pending_space = false;
    for &c in chars {
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && let Some(&left) = out.last() {
            let drop = (is_cjk_like(left) && is_cjk_like(c))
                || is_cjk_punct(c)
                || matches!(c, ',' | '.' | '!' | '?' | ';' | ':');
            if !drop {
                out.push(' ');
            }
        }
        out.push(c);
        pending_space = false;
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SentencePos {
    /// The next letter starts a sentence.
    Start,
    /// Inside a sentence.
    Mid,
    /// Right after ". ! ?"; a space turns this into `Start`, a letter means it was no end.
    AfterTerminal,
}

/// Upper-cases the first letter of each sentence in English text.
fn capitalize_sentences(chars: &mut [char]) {
    let mut pos = SentencePos::Start;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if pos == SentencePos::AfterTerminal {
                pos = SentencePos::Start;
            }
        } else if matches!(c, '.' | '!' | '?') {
            pos = SentencePos::AfterTerminal;
        } else if is_opening(c) || (pos == SentencePos::AfterTerminal && is_closing(c)) {
            // Quotes and brackets do not change the position.
        } else {
            if pos == SentencePos::Start && c.is_ascii_lowercase() {
                let end = word_end(chars, i);
                if !chars[i..end].iter().any(|c| c.is_uppercase()) {
                    chars[i] = c.to_ascii_uppercase();
                }
            }
            pos = SentencePos::Mid;
        }
        i += 1;
    }
}

fn add_final_punctuation(chars: &mut Vec<char>, lang: Language) {
    let Some(&last) = chars.last() else {
        return;
    };
    let before_last = chars.len().checked_sub(2).and_then(|i| chars.get(i));
    let done = if is_closing(last) {
        before_last.is_some_and(|c| is_sentence_end(*c))
    } else {
        is_sentence_end(last)
    };
    if !done {
        chars.push(match lang {
            Language::En => '.',
            Language::Ja => '。',
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DictionaryScope;

    const ALL: FormatOptions = FormatOptions {
        remove_fillers: true,
        add_final_punctuation: true,
        apply_dictionary: true,
    };
    const NONE: FormatOptions = FormatOptions {
        remove_fillers: false,
        add_final_punctuation: false,
        apply_dictionary: false,
    };

    fn en(raw: &str) -> String {
        format(raw, Language::En, &ALL, &[])
    }

    fn ja(raw: &str) -> String {
        format(raw, Language::Ja, &ALL, &[])
    }

    fn entry(written: &str, readings: &[&str], scope: DictionaryScope) -> DictionaryEntry {
        DictionaryEntry {
            id: format!("id-{written}"),
            written: written.to_owned(),
            readings: readings.iter().map(|r| (*r).to_owned()).collect(),
            scope,
        }
    }

    // --- is_cjk_char / is_cjk_punct ---

    #[test]
    fn cjk_classes() {
        for c in ['あ', 'ア', '漢', '々', 'ｱ', 'ー'] {
            assert!(is_cjk_char(c), "{c}");
            assert!(!is_cjk_punct(c), "{c}");
        }
        for c in ['、', '。', '「', '」', '！', '？', '\u{3000}', 'Ａ'] {
            assert!(is_cjk_punct(c), "{c}");
            assert!(!is_cjk_char(c), "{c}");
        }
        for c in ['a', '1', ' ', ',', 'é'] {
            assert!(!is_cjk_char(c), "{c}");
            assert!(!is_cjk_punct(c), "{c}");
        }
    }

    // --- is_hallucination ---

    #[test]
    fn hallucination_phrases_match_after_normalization() {
        for text in [
            "Thank you for watching.",
            "  thank you for watching!  ",
            "Thanks for watching",
            "Thank you.",
            "You",
            "you.",
            "[BLANK_AUDIO]",
            "[Music]",
            "(music)",
            "ご視聴ありがとうございました。",
            "ご清聴ありがとうございました",
            "字幕作成",
            "チャンネル登録よろしくお願いします。",
            "Thank  you   for watching",
        ] {
            assert!(is_hallucination(text), "{text:?}");
        }
    }

    #[test]
    fn real_speech_is_not_a_hallucination() {
        for text in [
            "",
            "   ",
            "...",
            "Thank you for the report.",
            "You are right.",
            "blank audio",
            "music",
            "今日はいい天気ですね。",
            "ご視聴ありがとうございました。次回もよろしく。",
        ] {
            assert!(!is_hallucination(text), "{text:?}");
        }
    }

    // --- English fillers ---

    #[test]
    fn english_fillers_with_commas() {
        assert_eq!(en("Um, hello there."), "Hello there.");
        assert_eq!(en("I think, um, that is right."), "I think, that is right.");
        assert_eq!(en("I went there, um."), "I went there.");
        assert_eq!(en("so um yeah"), "So yeah.");
        assert_eq!(en("Uh, uh, let me see."), "Let me see.");
        assert_eq!(en("Hmm. I see."), "I see.");
        assert_eq!(en("It was, erm, fine, uhh."), "It was, fine.");
        assert_eq!(en("I think um"), "I think.");
    }

    #[test]
    fn english_fillers_are_whole_words_only() {
        assert_eq!(
            en("Her umbrella is in the hummus."),
            "Her umbrella is in the hummus."
        );
        assert_eq!(en("The water is warmer."), "The water is warmer.");
        assert_eq!(en("The screw is 5 mm long."), "The screw is 5 mm long.");
        assert_eq!(en("She works in the ER."), "She works in the ER.");
    }

    #[test]
    fn english_fillers_kept_when_disabled() {
        let opts = FormatOptions {
            remove_fillers: false,
            ..ALL
        };
        assert_eq!(format("um, hello", Language::En, &opts, &[]), "Um, hello.");
    }

    // --- Japanese fillers ---

    #[test]
    fn japanese_fillers_between_punctuation() {
        assert_eq!(ja("えーと、今日は晴れです。"), "今日は晴れです。");
        assert_eq!(ja("今日は、えーと、晴れです。"), "今日は、晴れです。");
        assert_eq!(ja("今日は晴れです、うーん。"), "今日は晴れです。");
        assert_eq!(
            ja("えーっと、あのー、明日の会議は十時からです。"),
            "明日の会議は十時からです。"
        );
        assert_eq!(ja("んー、そうですね。"), "そうですね。");
        assert_eq!(ja("まー、いいでしょう。"), "いいでしょう。");
        assert_eq!(ja("あの〜 今日は"), "今日は。");
        assert_eq!(ja("うーん"), "");
        assert_eq!(ja("えーと。今日は晴れです。"), "今日は晴れです。");
    }

    #[test]
    fn japanese_fillers_inside_words_stay() {
        assert_eq!(ja("えーっとの話ではない。"), "えーっとの話ではない。");
        assert_eq!(ja("まーまー、落ち着いて。"), "まーまー、落ち着いて。");
        assert_eq!(ja("うーんと唸った。"), "うーんと唸った。");
    }

    // --- duplicate words ---

    #[test]
    fn duplicate_english_words_collapse() {
        assert_eq!(en("the the cat"), "The cat.");
        assert_eq!(en("The the the cat."), "The cat.");
        assert_eq!(en("I said the the."), "I said the.");
        assert_eq!(en("Go go go!"), "Go!");
    }

    #[test]
    fn non_duplicate_words_stay() {
        assert_eq!(en("1 1 2"), "1 1 2.");
        assert_eq!(en("the, the cat"), "The, the cat.");
        assert_eq!(en("then the cat"), "Then the cat.");
        assert_eq!(en("the theme"), "The theme.");
    }

    // --- whitespace ---

    #[test]
    fn cjk_spacing() {
        assert_eq!(ja("今日 は 晴れ です 。"), "今日は晴れです。");
        assert_eq!(ja("今日は、 晴れ です"), "今日は、晴れです。");
        assert_eq!(
            ja("Claude Code で 書きました。"),
            "Claude Code で書きました。"
        );
        assert_eq!(
            ja("今日は Claude Code を使った。"),
            "今日は Claude Code を使った。"
        );
        assert_eq!(ja("Claude Code 。"), "Claude Code。");
        assert_eq!(ja("「 こんにちは 」 と言った"), "「こんにちは」と言った。");
        assert_eq!(ja("今日は\u{3000}晴れ"), "今日は晴れ。");
    }

    #[test]
    fn whitespace_runs_collapse() {
        assert_eq!(en("  hello   \n world  "), "Hello world.");
        assert_eq!(en("hello , world ."), "Hello, world.");
        assert_eq!(en("wait ! what ?"), "Wait! What?");
    }

    // --- capitalization ---

    #[test]
    fn english_sentences_get_capitals() {
        assert_eq!(
            en("hello. how are you? fine! thanks"),
            "Hello. How are you? Fine! Thanks."
        );
        assert_eq!(en("\"quoted. words.\""), "\"Quoted. Words.\"");
        assert_eq!(
            en("iPhone is fine. macOS too."),
            "iPhone is fine. macOS too."
        );
        assert_eq!(en("version 3.14 is out. ok"), "Version 3.14 is out. Ok.");
        assert_eq!(en("see e.g. the docs"), "See e.g. The docs.");
        assert_eq!(en("wait... what"), "Wait... What.");
    }

    #[test]
    fn japanese_text_is_not_capitalized() {
        assert_eq!(
            ja("iphone を使う。claude code も。"),
            "iphone を使う。claude code も。"
        );
    }

    // --- final punctuation ---

    #[test]
    fn final_punctuation_on() {
        assert_eq!(en("hello"), "Hello.");
        assert_eq!(en("hello."), "Hello.");
        assert_eq!(en("hello!"), "Hello!");
        assert_eq!(en("hello?"), "Hello?");
        assert_eq!(en("hello..."), "Hello...");
        assert_eq!(en("hello,"), "Hello,");
        assert_eq!(en("he said \"hi.\""), "He said \"hi.\"");
        assert_eq!(en("he said \"hi\""), "He said \"hi\".");
        assert_eq!(en("see (figure 1)"), "See (figure 1).");
        assert_eq!(ja("今日は晴れです"), "今日は晴れです。");
        assert_eq!(ja("今日は晴れです。"), "今日は晴れです。");
        assert_eq!(ja("晴れですか？"), "晴れですか？");
        assert_eq!(ja("「こんにちは。」"), "「こんにちは。」");
        assert_eq!(ja("「こんにちは」"), "「こんにちは」。");
        assert_eq!(ja("Claude Code"), "Claude Code。");
        assert_eq!(en("今日"), "今日.");
    }

    #[test]
    fn final_punctuation_off() {
        let opts = FormatOptions {
            add_final_punctuation: false,
            ..ALL
        };
        assert_eq!(format("hello", Language::En, &opts, &[]), "Hello");
        assert_eq!(
            format("今日は晴れです", Language::Ja, &opts, &[]),
            "今日は晴れです"
        );
        assert_eq!(format("hello.", Language::En, &opts, &[]), "Hello.");
    }

    #[test]
    fn empty_and_blank_input() {
        assert_eq!(en(""), "");
        assert_eq!(en("   \n "), "");
        assert_eq!(ja(""), "");
        assert_eq!(format("", Language::En, &NONE, &[]), "");
    }

    #[test]
    fn all_rules_off_only_trims() {
        assert_eq!(
            format("  um, the the cat  ", Language::En, &NONE, &[]),
            "Um, the cat"
        );
        assert_eq!(format(" 今日 は ", Language::Ja, &NONE, &[]), "今日は");
    }

    // --- dictionary ---

    fn dict() -> Vec<DictionaryEntry> {
        vec![
            entry(
                "Claude Code",
                &["cloud code", "クロードコード"],
                DictionaryScope::Both,
            ),
            entry("Claude", &["cloud", "クロード"], DictionaryScope::Both),
            entry("Murmur", &["マーマー"], DictionaryScope::Ja),
            entry("GitHub", &["git hub", "github"], DictionaryScope::En),
        ]
    }

    #[test]
    fn dictionary_replaces_readings_case_insensitively() {
        assert_eq!(
            format(
                "I opened cloud code and GITHUB.",
                Language::En,
                &ALL,
                &dict()
            ),
            "I opened Claude Code and GitHub."
        );
        assert_eq!(
            format("Cloud Code is fast", Language::En, &ALL, &dict()),
            "Claude Code is fast."
        );
    }

    #[test]
    fn dictionary_longest_reading_first() {
        // "cloud code" (10 chars) wins over "cloud" (5 chars).
        assert_eq!(
            format("cloud code and cloud", Language::En, &ALL, &dict()),
            "Claude Code and Claude."
        );
    }

    #[test]
    fn dictionary_normalizes_the_written_form_itself() {
        assert_eq!(
            format("claude code rocks", Language::En, &ALL, &dict()),
            "Claude Code rocks."
        );
    }

    #[test]
    fn dictionary_respects_latin_word_boundaries() {
        assert_eq!(
            format("cloudy clouds cloud's", Language::En, &ALL, &dict()),
            "Cloudy clouds Claude's."
        );
        assert_eq!(
            format("githubber", Language::En, &ALL, &dict()),
            "Githubber."
        );
    }

    #[test]
    fn dictionary_japanese_readings_need_no_boundary() {
        assert_eq!(
            format(
                "クロードコードでマーマーを作った。",
                Language::Ja,
                &ALL,
                &dict()
            ),
            "Claude CodeでMurmurを作った。"
        );
        assert_eq!(
            format("クロードに聞いた。", Language::Ja, &ALL, &dict()),
            "Claudeに聞いた。"
        );
    }

    #[test]
    fn dictionary_scope_filters_by_language() {
        // "Murmur" is Ja only; "GitHub" is En only.
        assert_eq!(format("マーマー", Language::En, &ALL, &dict()), "マーマー.");
        assert_eq!(format("github", Language::Ja, &ALL, &dict()), "github。");
    }

    #[test]
    fn dictionary_off_or_empty_changes_nothing() {
        let opts = FormatOptions {
            apply_dictionary: false,
            ..ALL
        };
        assert_eq!(
            format("cloud code", Language::En, &opts, &dict()),
            "Cloud code."
        );
        assert_eq!(format("cloud code", Language::En, &ALL, &[]), "Cloud code.");
        let blank = vec![entry("  ", &["x"], DictionaryScope::Both)];
        assert_eq!(format("x marks", Language::En, &ALL, &blank), "X marks.");
    }

    // --- realistic samples ---

    #[test]
    fn realistic_japanese_haas_output() {
        let raw = "えーと、今日 は Claude Code で 新しい 機能 を 実装しました 。 うーん、テスト も 全部 通りました";
        assert_eq!(
            ja(raw),
            "今日は Claude Code で新しい機能を実装しました。テストも全部通りました。"
        );
    }

    #[test]
    fn realistic_english_parakeet_output() {
        let raw = "um so I opened the the pull request, uh, and it looks good. let me know what you think";
        assert_eq!(
            en(raw),
            "So I opened the pull request, and it looks good. Let me know what you think."
        );
    }

    // --- performance ---

    /// Run with `cargo test -p murmur-core --release -- --ignored`.
    #[test]
    #[ignore = "timing; run in release"]
    fn format_2000_chars_under_2ms() {
        let unit = "えーと、今日 は Claude Code で 新しい 機能 を 実装しました 。 um, the the tests pass, uh. ";
        let mut raw = String::new();
        while raw.chars().count() < 2_000 {
            raw.push_str(unit);
        }
        let dictionary = dict();
        let mut times = Vec::new();
        for _ in 0..21 {
            let start = std::time::Instant::now();
            let out = format(&raw, Language::Ja, &ALL, &dictionary);
            times.push(start.elapsed());
            assert!(!out.is_empty());
        }
        times.sort();
        let median = times[times.len() / 2];
        assert!(median.as_micros() < 2_000, "median {median:?}");
    }
}
