//! Text statistics: tokens, sentences, syllables, character classes.
//!
//! Segmentation is script-aware. Latin text splits on whitespace and punctuation; CJK
//! ideographs and kana have no spaces, so each character counts as one token, and sentences end
//! at CJK punctuation as well as at Latin `.` `!` `?`. All offsets are byte offsets into the
//! text that was analyzed, so callers can highlight the exact ranges.

/// A word-like unit: a Latin/numeric run, or a single CJK character.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub start: usize,
    pub end: usize,
    /// Lowercased word text (Latin) or the single character (CJK); object placeholders
    /// (U+FFFC) and pure punctuation never become tokens.
    pub text: String,
    /// Syllable estimate (Latin); CJK tokens count as one "syllable" for the formulas that
    /// would otherwise divide by zero.
    pub syllables: usize,
    pub cjk: bool,
}

/// A sentence: a byte range plus the aggregate counts its readability formulas need.
#[derive(Clone, Debug, PartialEq)]
pub struct Sentence {
    pub start: usize,
    pub end: usize,
    /// Index range into [`TextStats::tokens`].
    pub tok0: usize,
    pub tok1: usize,
    /// Non-whitespace characters, counting each CJK character (a fairer "length" for CJK).
    pub chars: usize,
}

/// Aggregate statistics over one text.
#[derive(Clone, Debug, Default)]
pub struct TextStats {
    pub tokens: Vec<Token>,
    pub sentences: Vec<Sentence>,
    /// All characters except newlines (what "characters with spaces" counts).
    pub chars: usize,
    /// Latin letters and digits only (the `L` of Coleman-Liau).
    pub letters: usize,
    pub cjk_chars: usize,
    pub syllables: usize,
    /// Tokens with three or more syllables (SMOG counts these).
    pub polysyllables: usize,
    /// Latin tokens with three or more syllables (Gunning Fog's "complex words").
    pub complex_words: usize,
}

impl TextStats {
    pub fn compute(text: &str) -> TextStats {
        let tokens = tokenize(text);
        let sentences = split_sentences(text, &tokens);
        let mut s = TextStats { tokens, sentences, ..TextStats::default() };
        for c in text.chars() {
            if c == '\n' {
                continue;
            }
            s.chars += 1;
            if c.is_ascii_alphabetic() || c.is_ascii_digit() {
                s.letters += 1;
            }
            if is_cjk(c) {
                s.cjk_chars += 1;
            }
        }
        for t in &s.tokens {
            s.syllables = s.syllables.saturating_add(t.syllables);
            if t.syllables >= 3 {
                s.polysyllables += 1;
                if !t.cjk {
                    s.complex_words += 1;
                }
            }
        }
        s
    }

    /// Latin/numeric words plus CJK characters — the "words" a word count shows.
    pub fn word_count(&self) -> usize {
        self.tokens.len()
    }

    pub fn latin_words(&self) -> usize {
        self.tokens.iter().filter(|t| !t.cjk).count()
    }
}

/// CJK ideographs (including extensions), kana and the CJK punctuation used in sentence rules.
/// Hangul is excluded: Korean writes words with spaces, so it flows through the Latin path.
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x2E80..=0x2FDF      // radicals and ideographic description characters
        | 0x3000..=0x303F    // CJK symbols and punctuation
        | 0x3041..=0x30FF    // hiragana + katakana
        | 0x3190..=0x319F    // kanbun annotation marks
        | 0x3400..=0x4DBF    // CJK extension A
        | 0x4E00..=0x9FFF    // the URO (common ideographs)
        | 0xF900..=0xFAFF    // compatibility ideographs
        | 0x20000..=0x2FA1F  // extensions B..F and compatibility supplement
    )
}

/// Is `c` a Latin sentence ender? (CJK enders are handled separately in [`split_sentences`].)
fn is_ender(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
}

/// Closing punctuation that may directly follow a sentence ender (`."` `）` `」` …).
fn is_closer(c: char) -> bool {
    matches!(c, '"' | '\'' | ')' | ']' | '”' | '’' | '）' | '」' | '』' | '】' | '〉' | '》')
}

/// Tokens: maximal Latin letter/digit runs (apostrophes and hyphens kept inside a word), one
/// token per CJK character. Byte offsets are preserved for highlighting.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap_or('\0');
        if c == '\0' {
            break;
        }
        let start = i;
        if is_cjk(c) {
            // One character, one token; CJK punctuation is skipped as separator below.
            if !matches!(c as u32, 0x3000..=0x303F) {
                out.push(Token {
                    start,
                    end: i + c.len_utf8(),
                    text: c.to_string(),
                    syllables: 1,
                    cjk: true,
                });
            }
            i += c.len_utf8();
            continue;
        }
        if c.is_alphanumeric() || c == '\'' || c == '-' || c == '’' {
            // A Latin word: keep internal apostrophes/hyphens, trim them from the token text.
            let mut end = i;
            for (j, wc) in text[i..].char_indices() {
                if wc.is_alphanumeric() || wc == '\'' || wc == '-' || wc == '’' {
                    end = i + j + wc.len_utf8();
                } else {
                    break;
                }
            }
            let raw = &text[start..end];
            let word = raw.trim_matches(|c: char| c == '\'' || c == '-' || c == '’');
            if word.chars().any(|c| c.is_alphanumeric()) {
                out.push(Token { start, end, text: word.to_lowercase(), syllables: syllables(word), cjk: false });
            }
            i = end;
            continue;
        }
        i += c.len_utf8();
    }
    out
}

/// English syllable estimate: contiguous vowel groups (y counts when not the first letter),
/// one group = one syllable; a silent trailing `e` doesn't count; at least one. Digits count
/// as one syllable per run ("2024" reads as one beat in the formulas).
pub fn syllables(word: &str) -> usize {
    let w: Vec<char> = word.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if w.is_empty() {
        return 0;
    }
    if w.iter().all(|c| c.is_ascii_digit()) {
        return 1;
    }
    let is_vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y');
    let mut count = 0usize;
    let mut prev_vowel = false;
    for (k, &c) in w.iter().enumerate() {
        let v = is_vowel(c) && !(c == 'y' && k == 0);
        if v && !prev_vowel {
            count += 1;
        }
        prev_vowel = v;
    }
    // Silent 'e': "time" is one syllable, but "table" keeps its 'le' syllable.
    if count > 1 && w.len() >= 3 {
        let last = w[w.len() - 1];
        let prev = w[w.len() - 2];
        if last == 'e' && !is_vowel(prev) && !(prev == 'l') {
            count -= 1;
        }
    }
    count.max(1)
}

/// Common abbreviations whose `.` does not end a sentence.
const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "st", "jr", "sr", "vs", "etc", "eg", "ie", "fig", "no", "inc",
    "ltd", "co", "corp", "approx", "dept", "est", "min", "max", "al", "ca", "cf",
];

/// Split into sentences. Latin enders need confirmation (abbreviation list, digits or lowercase
/// right after, a closer glued to the ender); CJK enders always end. `\n` is a hard boundary.
pub fn split_sentences(text: &str, tokens: &[Token]) -> Vec<Sentence> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = text[i..].chars().next().unwrap_or('\n');
        if c == '\n' {
            push_sentence(text, tokens, start, i, &mut out);
            i += 1;
            start = i;
            continue;
        }
        if is_ender(c) {
            let after = i + c.len_utf8();
            if latin_ender_confirmed(text, bytes, i, after) {
                // Include any closing quotes/brackets glued to the ender.
                let mut end = after;
                while let Some(nc) = text[end..].chars().next() {
                    if is_closer(nc) {
                        end += nc.len_utf8();
                    } else {
                        break;
                    }
                }
                push_sentence(text, tokens, start, end, &mut out);
                i = end;
                start = i;
                continue;
            }
        }
        if c == '…' || c == '‥' || matches!(c, '。' | '！' | '？') {
            let mut end = i + c.len_utf8();
            while let Some(nc) = text[end..].chars().next() {
                if is_closer(nc) {
                    end += nc.len_utf8();
                } else {
                    break;
                }
            }
            push_sentence(text, tokens, start, end, &mut out);
            i = end;
            start = i;
            continue;
        }
        i += c.len_utf8();
    }
    push_sentence(text, tokens, start, bytes.len(), &mut out);
    out
}

/// A Latin ender ends the sentence unless it is an abbreviation's dot, sits between digits
/// ("3.5"), follows a single uppercase initial ("J."), or has a lowercase letter glued on
/// (a URL's "example.com"). A closing quote or bracket after it keeps it a sentence end.
fn latin_ender_confirmed(text: &str, bytes: &[u8], at: usize, after: usize) -> bool {
    // Digits both sides: a decimal point.
    if at > 0 && bytes[at - 1].is_ascii_digit() && after < bytes.len() && bytes[after].is_ascii_digit() {
        return false;
    }
    // The word before the dot: its original first letter (initials like "J.") and its
    // lowercase form (the abbreviation list).
    let mut word = String::new();
    let mut first_char: Option<char> = None;
    let mut k = at;
    while k > 0 {
        let c = text[..k].chars().last().unwrap_or(' ');
        if c.is_ascii_alphabetic() {
            if first_char.is_none() {
                first_char = Some(c);
            }
            word.insert(0, c.to_ascii_lowercase());
            k -= c.len_utf8();
        } else {
            break;
        }
    }
    let is_initial = word.len() == 1 && first_char.is_some_and(|c| c.is_ascii_uppercase());
    if is_initial || ABBREVIATIONS.contains(&word.as_str()) {
        return false;
    }
    // What follows: end of text, whitespace or a closer means yes; a letter (lowercase URLs,
    // "e.g,"-style continuation) means no.
    match text[after..].chars().next() {
        None => true,
        Some(nc) if nc.is_whitespace() || is_closer(nc) => true,
        Some(nc) => nc.is_uppercase(),
    }
}

/// Build a `Sentence` from a byte range, linking it to its token range.
fn push_sentence(text: &str, tokens: &[Token], start: usize, end: usize, out: &mut Vec<Sentence>) {
    let tok0 = tokens.partition_point(|t| t.end <= start);
    let tok1 = tokens.partition_point(|t| t.start < end).max(tok0);
    let mut chars = 0usize;
    for c in text[start..end.min(text.len())].chars() {
        if !c.is_whitespace() {
            chars += 1;
        }
    }
    if end > start && (chars > 0 || tok1 > tok0) {
        out.push(Sentence { start, end, tok0, tok1, chars });
    }
}

/// English function words excluded from the "top words" table (the classic NLTK list, trimmed).
pub const STOPWORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "be", "been", "but", "by", "can", "could", "did", "do", "does",
    "for", "from", "had", "has", "have", "he", "her", "hers", "him", "his", "i", "if", "in", "into",
    "is", "it", "its", "just", "me", "my", "no", "nor", "not", "of", "on", "or", "our", "ours",
    "she", "should", "so", "some", "than", "that", "the", "their", "them", "then", "there", "these",
    "they", "this", "those", "to", "too", "up", "us", "was", "we", "were", "what", "when", "where",
    "which", "who", "whom", "why", "will", "with", "would", "you", "your", "yours",
];

#[cfg(test)]
mod quick {
    use super::*;

    #[test]
    fn syllable_basics() {
        assert_eq!(syllables("time"), 1);
        assert_eq!(syllables("table"), 2);
        assert_eq!(syllables("readability"), 5);
        assert_eq!(syllables("2024"), 1);
    }

    #[test]
    fn cjk_and_latin_tokens() {
        let ts = TextStats::compute("Hello world. 你好，世界！");
        assert_eq!(ts.latin_words(), 2);
        assert_eq!(ts.cjk_chars, 4);
        assert_eq!(ts.sentences.len(), 2);
    }

    #[test]
    fn abbreviations_do_not_split() {
        let ts = TextStats::compute("Dr. Smith met Mr. Lee at 3.5 km. They talked.");
        assert_eq!(ts.sentences.len(), 2);
    }
}
