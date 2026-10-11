//! Writing-issue detectors: local, rule-based checks over one paragraph's text.
//!
//! Every finding carries the byte range of the offending text inside its paragraph, so the
//! engine command can turn it into a document position and the UI can jump there. The rules are
//! deliberately conservative (fewer false positives beats exhaustive detection) and only cover
//! English prose; CJK documents get the long-sentence check on character counts.

use crate::stats::{self, TextStats};

/// The kinds of issue the detectors report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingKind {
    /// A sentence far past a comfortable length.
    LongSentence,
    /// A likely passive construction ("was written").
    PassiveVoice,
    /// A wordy phrase with a shorter standard replacement.
    WordyPhrase,
    /// A filler adverb that usually weakens the sentence.
    FillerAdverb,
    /// The same word twice in a row ("the the").
    RepeatedWord,
    /// Several sentences in a row starting with the same word.
    RepeatedStart,
}

/// One detected issue, anchored in its paragraph.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    /// Body block path of the paragraph (`[block]`, or `[block, row, cell, …]` in a table);
    /// filled by the report builder, empty when the detector runs standalone.
    #[serde(default)]
    pub path: Vec<u32>,
    /// Byte range within the paragraph's text.
    pub start: usize,
    pub end: usize,
    pub message: String,
    /// Shorter alternative, for wordy phrases.
    pub suggestion: Option<String>,
}

/// Latin sentences above this many tokens are flagged.
const MAX_SENTENCE_WORDS: usize = 45;
/// CJK sentences above this many characters are flagged (Chinese editors' rule of thumb).
const MAX_SENTENCE_CJK_CHARS: usize = 70;
/// How far past a be-verb a participle may sit for a passive match.
const PASSIVE_WINDOW: usize = 3;
/// How many consecutive same-word sentence starts it takes to flag.
const START_REPEAT: usize = 3;

/// Be-forms that can head a passive.
const BE_FORMS: &[&str] =
    &["am", "is", "are", "was", "were", "be", "been", "being", "isn't", "aren't", "wasn't", "weren't"];

/// Irregular past participles (the common ones); regular participles end in "ed".
const IRREGULAR_PARTICIPLES: &[&str] = &[
    "arisen", "awoken", "begun", "bent", "bitten", "blown", "born", "brought", "broken", "built",
    "bought", "caught", "chosen", "done", "drawn", "driven", "drunk", "eaten", "fallen", "fed",
    "felt", "fought", "found", "flown", "forbidden", "forgotten", "forgiven", "frozen", "given",
    "gone", "grown", "heard", "hidden", "held", "hurt", "kept", "known", "laid", "led", "left",
    "lost", "made", "meant", "met", "paid", "put", "read", "ridden", "risen", "said", "seen",
    "sold", "sent", "set", "shaken", "shot", "shown", "shut", "sung", "sunk", "sat", "slept",
    "spoken", "spent", "split", "spread", "stood", "stolen", "struck", "sworn", "taken", "taught",
    "told", "thought", "thrown", "understood", "woken", "worn", "won", "written",
];

/// Filler adverbs: usually safe to delete, so each one is a nudge, not an error.
const FILLER_ADVERBS: &[&str] = &[
    "very", "really", "quite", "rather", "just", "actually", "basically", "literally", "simply",
    "totally", "definitely", "certainly", "absolutely", "extremely", "obviously", "clearly",
    "seriously", "honestly", "truly", "somewhat", "somehow",
];

/// Wordy phrases and their plain replacements (the classic plain-language list, trimmed to the
/// unambiguous ones). An empty replacement means "just delete it".
const WORDY_PHRASES: &[(&str, &str)] = &[
    ("due to the fact that", "because"),
    ("in order to", "to"),
    ("at this point in time", "now"),
    ("at the present time", "now"),
    ("in the event that", "if"),
    ("for the reason that", "because"),
    ("in spite of the fact that", "although"),
    ("despite the fact that", "although"),
    ("on a daily basis", "daily"),
    ("on a regular basis", "regularly"),
    ("a large number of", "many"),
    ("a large proportion of", "many"),
    ("a majority of", "most"),
    ("have the ability to", "can"),
    ("is able to", "can"),
    ("are able to", "can"),
    ("make a decision", "decide"),
    ("prior to", "before"),
    ("subsequent to", "after"),
    ("in the near future", "soon"),
    ("with regard to", "about"),
    ("in relation to", "about"),
    ("with the exception of", "except"),
    ("on the grounds that", "because"),
    ("in the vicinity of", "near"),
    ("until such time as", "until"),
    ("by means of", "by"),
    ("in accordance with", "under"),
    ("at all times", "always"),
    ("it is important to note that", ""),
    ("each and every", "every"),
    ("first and foremost", "first"),
    ("completely eliminate", "eliminate"),
    ("past history", "history"),
    ("end result", "result"),
];

/// Run every detector over one paragraph. `stats` comes from [`TextStats::compute`] so the
/// paragraph's statistics and findings share one segmentation.
pub fn detect(_text: &str, stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    out.extend(long_sentences(stats));
    out.extend(wordy_phrases(stats));
    out.extend(filler_adverbs(stats));
    out.extend(repeated_words(stats));
    out.extend(passive_voice(stats));
    out.extend(repeated_starts(stats));
    out
}

/// Sentences well past a comfortable reading length. A CJK-heavy sentence uses the character
/// rule, a Latin one the word rule.
fn long_sentences(stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    for s in &stats.sentences {
        let words = s.tok1 - s.tok0;
        let cjk = stats.tokens.get(s.tok0..s.tok1).map(|t| t.iter().filter(|x| x.cjk).count()).unwrap_or(0);
        let cjk_heavy = cjk * 3 > words;
        let too_long = if cjk_heavy { s.chars > MAX_SENTENCE_CJK_CHARS } else { words > MAX_SENTENCE_WORDS };
        if !too_long {
            continue;
        }
        let bytes = s.end.saturating_sub(s.start);
        let measure = if cjk_heavy { format!("{} chars", s.chars) } else { format!("{words} words") };
        out.push(Finding {
            kind: FindingKind::LongSentence,
                path: Vec::new(),
            start: s.start,
            end: s.end,
            message: format!("Long sentence: {measure}, {bytes} bytes. Consider splitting it."),
            suggestion: None,
        });
    }
    out
}

/// Wordy phrases, matched case-insensitively on consecutive Latin tokens.
fn wordy_phrases(stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    for ti in 0..stats.tokens.len() {
        let first = &stats.tokens[ti];
        if first.cjk {
            continue;
        }
        for (phrase, fix) in WORDY_PHRASES {
            let parts: Vec<&str> = phrase.split(' ').collect();
            let mut matched = true;
            for (k, part) in parts.iter().enumerate() {
                match stats.tokens.get(ti + k) {
                    Some(tok) if !tok.cjk && tok.text == *part => {}
                    _ => {
                        matched = false;
                        break;
                    }
                }
            }
            if !matched {
                continue;
            }
            let Some(last) = stats.tokens.get(ti + parts.len() - 1) else { continue };
            out.push(Finding {
                kind: FindingKind::WordyPhrase,
                path: Vec::new(),
                start: first.start,
                end: last.end,
                message: format!("Wordy phrase “{phrase}”"),
                suggestion: Some(if fix.is_empty() { "(delete)".to_string() } else { format!("Consider “{fix}”") }),
            });
        }
    }
    out
}

/// Filler adverbs (single tokens, whole-word match).
fn filler_adverbs(stats: &TextStats) -> Vec<Finding> {
    stats
        .tokens
        .iter()
        .filter(|t| !t.cjk && FILLER_ADVERBS.contains(&t.text.as_str()))
        .map(|t| Finding {
            kind: FindingKind::FillerAdverb,
                path: Vec::new(),
            start: t.start,
            end: t.end,
            message: format!("Filler adverb “{}” — usually safe to delete", t.text),
            suggestion: Some("(delete)".to_string()),
        })
        .collect()
}

/// The same word twice in a row (skips one-letter words, hyphenated words and CJK, where
/// doubling can be valid).
fn repeated_words(stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    let toks = &stats.tokens;
    let mut i = 1;
    while i < toks.len() {
        let (a, b) = (&toks[i - 1], &toks[i]);
        if !a.cjk && !b.cjk && a.text == b.text && a.text.len() > 1 && !a.text.contains('-') {
            out.push(Finding {
                kind: FindingKind::RepeatedWord,
                path: Vec::new(),
                start: a.start,
                end: b.end,
                message: format!("Repeated word “{}”", a.text),
                suggestion: Some("(delete one)".to_string()),
            });
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Passive voice: a be-form with a participle within the next few words (the participle may
/// follow an adverb or another be-form belongs to the next verb chain). Regular participles
/// must end in "ed" and be long enough to not be a bare verb like "need".
fn passive_voice(stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    let toks = &stats.tokens;
    if toks.is_empty() {
        return out;
    }
    for (i, t) in toks.iter().enumerate() {
        if t.cjk || !BE_FORMS.contains(&t.text.as_str()) {
            continue;
        }
        for w in toks.iter().take((i + PASSIVE_WINDOW).min(toks.len() - 1)).skip(i + 1) {
            if w.cjk || BE_FORMS.contains(&w.text.as_str()) {
                break;
            }
            let regular = w.text.ends_with("ed") && w.text.len() > 4;
            if IRREGULAR_PARTICIPLES.contains(&w.text.as_str()) || regular {
                out.push(Finding {
                    kind: FindingKind::PassiveVoice,
                    path: Vec::new(),
                    start: t.start,
                    end: w.end,
                    message: format!("Possible passive voice (“{} {}”)", t.text, w.text),
                    suggestion: Some("Prefer the active voice".to_string()),
                });
                break;
            }
        }
    }
    out
}

/// Three or more consecutive sentences starting with the same (non-stopword) word.
fn repeated_starts(stats: &TextStats) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut run: Vec<(usize, usize)> = Vec::new(); // (sentence index, start-token index)
    for (si, s) in stats.sentences.iter().enumerate() {
        let first = stats.tokens.get(s.tok0);
        let usable = first.is_some_and(|t| !t.cjk && t.text.len() > 2 && !stats::STOPWORDS.contains(&t.text.as_str()));
        let same = usable && run.last().is_some_and(|&(_, r0)| {
            match (stats.tokens.get(r0), first) {
                (Some(a), Some(b)) => a.text == b.text,
                _ => false,
            }
        });
        if same {
            run.push((si, s.tok0));
        } else {
            flush_start_run(stats, &mut run, &mut out);
            if usable {
                run.push((si, s.tok0));
            }
        }
    }
    flush_start_run(stats, &mut run, &mut out);
    out
}

/// Emit one finding if a run of same-word sentence starts reached the threshold.
fn flush_start_run(stats: &TextStats, run: &mut Vec<(usize, usize)>, out: &mut Vec<Finding>) {
    if run.len() >= START_REPEAT {
        let first = run[0].0;
        let last = run[run.len() - 1].0;
        let word = stats.tokens.get(run[0].1).map(|t| t.text.clone()).unwrap_or_default();
        out.push(Finding {
            kind: FindingKind::RepeatedStart,
                path: Vec::new(),
            start: stats.sentences[first].start,
            end: stats.sentences[last].end,
            message: format!("{} sentences in a row start with “{}”", run.len(), word),
            suggestion: Some("Vary the sentence openers".to_string()),
        });
    }
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(text: &str) -> Vec<Finding> {
        let stats = TextStats::compute(text);
        detect(text, &stats)
    }

    #[test]
    fn detects_repeated_word() {
        let f = findings("I saw the the dog in the park yesterday afternoon with my friend.");
        assert!(f.iter().any(|f| f.kind == FindingKind::RepeatedWord));
    }

    #[test]
    fn detects_wordy_phrase() {
        let f = findings("Due to the fact that it rained, we stayed home and read books quietly together.");
        assert!(f.iter().any(|f| f.kind == FindingKind::WordyPhrase));
    }

    #[test]
    fn detects_passive() {
        let f = findings("The report was written by the committee last week and it was long.");
        assert!(f.iter().any(|f| f.kind == FindingKind::PassiveVoice));
    }

    #[test]
    fn detects_filler() {
        let f = findings("The result was really quite good actually for a first attempt.");
        assert!(f.iter().any(|f| f.kind == FindingKind::FillerAdverb));
    }

    #[test]
    fn detects_long_sentence() {
        let words = "word ".repeat(60);
        let f = findings(&format!("{}end.", words));
        assert!(f.iter().any(|f| f.kind == FindingKind::LongSentence));
    }

    #[test]
    fn hostile_input_yields_findings_not_panics() {
        // Empty, lone punctuation, lone CJK, only objects: none may panic or divide by zero.
        for t in ["", ".", "!!!", "。", "测试", "\u{FFFC}", "a", "- - -", "e.g."] {
            let _ = findings(t);
        }
    }
}
