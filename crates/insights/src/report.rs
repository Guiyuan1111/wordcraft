//! The insights report: the serde model shared by the engine command, the CLI and the UI pane,
//! plus plain-text and Markdown renderers.
//!
//! Counting aggregates body paragraphs plus the "extra" paragraphs (notes, text boxes) when the
//! caller asks for them. Findings and the longest-sentence list are anchored to body block
//! paths only, because those are the positions a document jump can address.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::detectors::{self, Finding, FindingKind};
use crate::metrics::{self, Readability, Script};
use crate::stats::{self, TextStats};

/// One paragraph handed to [`build`]: its body block path and text.
#[derive(Clone, Debug)]
pub struct ParaText {
    /// Block path as `Document` paths go (`[block]`, or `[block, row, cell, …]` in a table).
    pub path: Vec<u32>,
    pub text: String,
}

/// Where a flagged sentence lives, for jump-to-text in the UI.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SentenceRef {
    /// Body block path of the paragraph holding the sentence.
    pub path: Vec<u32>,
    /// Byte range within that paragraph's text.
    pub start: usize,
    pub end: usize,
    pub words: usize,
    /// The sentence's own text, trimmed, for the list.
    pub snippet: String,
}

/// A content word and how often it appears (stop words excluded).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WordFreq {
    pub word: String,
    pub count: usize,
}

/// One bucket of the sentence-length histogram.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Bucket {
    /// Upper bound in words (`5`, `10`, … `45`, the last bucket is "46+").
    pub upto: u32,
    pub count: usize,
}

/// A pre-rendered headline: a number plus its plain-language band, so the UI panel stays
/// "dumb" — it reads JSON and never links this crate.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Headline {
    pub score: f32,
    pub label: String,
}

/// The full analysis of one document or text.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct InsightsReport {
    /// Latin/numeric words plus CJK characters — the "words" a word count shows.
    pub words: usize,
    pub latin_words: usize,
    pub cjk_chars: usize,
    pub characters: usize,
    pub sentences: usize,
    pub paragraphs: usize,
    /// Tables counted as blocks (a rough structural figure next to `paragraphs`).
    pub tables: usize,
    /// Silent reading time at 238 wpm for Latin words and 300 chars/min for CJK (Brysbaert 2019
    /// meta-analysis and standard Chinese reading-rate research).
    pub reading_minutes: f32,
    /// Speaking time at 150 wpm Latin / 220 chars/min CJK.
    pub speaking_minutes: f32,
    pub avg_sentence_words: f32,
    /// Average non-whitespace characters per sentence — the length measure CJK style guides use.
    pub avg_sentence_chars: f32,
    pub script: Script,
    pub readability: Readability,
    /// Flesch Reading Ease score and its band; `None` when the text is too short or CJK-heavy
    /// for the Latin formulas.
    pub flesch: Option<Headline>,
    /// The school-grade band the text roughly needs, averaged over the grade-style metrics.
    pub grade: Option<Headline>,
    /// Sentence lengths in words, in document order (the UI draws its histogram from this).
    pub sentence_lengths: Vec<usize>,
    /// The five longest sentences, longest first, with jump positions.
    pub longest_sentences: Vec<SentenceRef>,
    pub histogram: Vec<Bucket>,
    /// Up to 20 most frequent content words, most frequent first.
    pub top_words: Vec<WordFreq>,
    /// Writing issues found in body paragraphs, document order.
    pub findings: Vec<Finding>,
    /// Findings per kind, so the UI can show counts without re-grouping.
    pub finding_counts: Vec<(FindingKind, usize)>,
}

/// Analyse paragraphs: `body` supplies everything; `extra` paragraphs (notes, text boxes) add to
/// the counts but never to findings or jump targets.
pub fn build(body: &[ParaText], extra: &[ParaText]) -> InsightsReport {
    let mut all_stats = TextStats::default();
    let mut body_stats_per_para: Vec<(Vec<u32>, TextStats)> = Vec::with_capacity(body.len());
    let mut tables = 0usize;

    for p in body {
        if is_table_path(&p.path) {
            tables += 1;
        }
        let st = TextStats::compute(&p.text);
        all_stats.tokens.extend(st.tokens.iter().cloned());
        all_stats.sentences.extend(st.sentences.iter().cloned());
        all_stats.chars += st.chars;
        all_stats.letters += st.letters;
        all_stats.cjk_chars += st.cjk_chars;
        all_stats.syllables += st.syllables;
        all_stats.polysyllables += st.polysyllables;
        all_stats.complex_words += st.complex_words;
        body_stats_per_para.push((p.path.clone(), st));
    }
    for p in extra {
        let st = TextStats::compute(&p.text);
        all_stats.chars += st.chars;
        all_stats.letters += st.letters;
        all_stats.cjk_chars += st.cjk_chars;
        all_stats.syllables += st.syllables;
        all_stats.polysyllables += st.polysyllables;
        all_stats.complex_words += st.complex_words;
        all_stats.tokens.extend(st.tokens.iter().cloned());
        // Extra sentences join the histogram but their paragraph offsets mean nothing in the
        // body, so they stay out of `longest_sentences`.
    }

    let script = metrics::dominant_script(&all_stats);
    let readability = metrics::evaluate(&all_stats);
    let (avg_w, avg_c) = metrics::averages(&all_stats);

    let sentence_lengths: Vec<usize> = all_stats.sentences.iter().map(|s| s.tok1 - s.tok0).collect();
    let histogram = histogram(&sentence_lengths);
    let longest = longest_sentences(&body_stats_per_para, 5);
    let top_words = top_words(&all_stats, 20);

    let mut findings: Vec<Finding> = Vec::new();
    for (path, st) in &body_stats_per_para {
        for mut f in detectors::detect("", st) {
            f.path = path.clone();
            findings.push(f);
        }
    }
    findings.sort_by_key(|f| (f.start, f.end));
    let finding_counts = counts_by_kind(&findings);

    let (latin, words) = (all_stats.latin_words(), all_stats.word_count());
    let reading_minutes = latin as f32 / 238.0 + all_stats.cjk_chars as f32 / 300.0;
    let speaking_minutes = latin as f32 / 150.0 + all_stats.cjk_chars as f32 / 220.0;
    let flesch =
        readability.flesch.map(|f| Headline { score: f, label: metrics::flesch_label(f).into() });
    let grade =
        metrics::grade_band(&readability).map(|(g, l)| Headline { score: g, label: l.into() });

    InsightsReport {
        words,
        latin_words: latin,
        cjk_chars: all_stats.cjk_chars,
        characters: all_stats.chars,
        sentences: all_stats.sentences.len(),
        paragraphs: body_stats_per_para.len(),
        tables,
        reading_minutes,
        speaking_minutes,
        avg_sentence_words: avg_w,
        avg_sentence_chars: avg_c,
        script,
        readability,
        flesch,
        grade,
        sentence_lengths,
        longest_sentences: longest,
        histogram,
        top_words,
        findings,
        finding_counts,
    }
}

/// Table cells sit deeper than one path element; any deeper paragraph counts as "in a table".
fn is_table_path(path: &[u32]) -> bool {
    path.len() > 1
}

/// Nine-bucket sentence-length histogram: 1–5, 6–10, …, 41–45, 46+ words.
fn histogram(lengths: &[usize]) -> Vec<Bucket> {
    let mut out = Vec::new();
    for k in 0..9u32 {
        let lo = (k * 5 + 1) as usize;
        let upto = (k + 1) * 5;
        let count = lengths.iter().filter(|&&l| l >= lo && l <= upto as usize).count();
        out.push(Bucket { upto, count });
    }
    out
}

/// The `n` longest body sentences, longest first.
fn longest_sentences(paras: &[(Vec<u32>, TextStats)], n: usize) -> Vec<SentenceRef> {
    let mut all: Vec<SentenceRef> = Vec::new();
    for (path, st) in paras {
        for s in &st.sentences {
            let words = s.tok1 - s.tok0;
            if words == 0 {
                continue;
            }
            let snippet = st
                .tokens
                .get(s.tok0..s.tok1)
                .map(|t| t.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            all.push(SentenceRef { path: path.clone(), start: s.start, end: s.end, words, snippet });
        }
    }
    all.sort_by(|a, b| b.words.cmp(&a.words).then(a.start.cmp(&b.start)));
    all.truncate(n);
    all
}

/// Most frequent content words (Latin only; CJK character frequency is dominated by function
/// characters and would mislead without proper word segmentation).
fn top_words(stats: &TextStats, n: usize) -> Vec<WordFreq> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in &stats.tokens {
        if t.cjk || t.text.len() < 3 || stats::STOPWORDS.contains(&t.text.as_str()) {
            continue;
        }
        *counts.entry(t.text.as_str()).or_default() += 1;
    }
    let mut v: Vec<WordFreq> = counts.into_iter().map(|(word, count)| WordFreq { word: word.to_string(), count }).collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then(a.word.cmp(&b.word)));
    v.truncate(n);
    v
}

fn counts_by_kind(findings: &[Finding]) -> Vec<(FindingKind, usize)> {
    let kinds = [
        FindingKind::LongSentence,
        FindingKind::PassiveVoice,
        FindingKind::WordyPhrase,
        FindingKind::FillerAdverb,
        FindingKind::RepeatedWord,
        FindingKind::RepeatedStart,
    ];
    kinds
        .map(|k| {
            let count = findings.iter().filter(|f| f.kind == k).count();
            (k, count)
        })
        .into_iter()
        .filter(|(_, c)| *c > 0)
        .collect()
}

/// One line per finding kind present, then the top issues, for `wordcraft-cli insights`.
pub fn to_text(r: &InsightsReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Writing Insights");
    let _ = writeln!(s, "================");
    let _ = writeln!(s, "words {} (latin {}, cjk chars {}) · characters {} · sentences {} · paragraphs {}",
        r.words, r.latin_words, r.cjk_chars, r.characters, r.sentences, r.paragraphs);
    let _ = writeln!(s, "avg sentence {:.1} words / {:.1} chars · reading ~{:.1} min · speaking ~{:.1} min",
        r.avg_sentence_words, r.avg_sentence_chars, r.reading_minutes, r.speaking_minutes);
    if let Some(f) = r.readability.flesch {
        let _ = writeln!(s, "Flesch ease {:.0} ({})", f, metrics::flesch_label(f));
    }
    if let Some((grade, label)) = metrics::grade_band(&r.readability) {
        let _ = writeln!(s, "grade level ~{:.1} ({})", grade, label);
    }
    let _ = writeln!(s, "issues: {}", r.finding_counts.iter().map(|(k, n)| format!("{k:?} {n}")).collect::<Vec<_>>().join(", "));
    for f in r.findings.iter().take(20) {
        let _ = writeln!(s, "  [path {:?} @{}..{}] {}", f.path, f.start, f.end, f.message);
    }
    s
}

/// GitHub-flavoured Markdown rendering, for pasting into notes.
pub fn to_markdown(r: &InsightsReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Writing Insights\n");
    let _ = writeln!(s, "| Metric | Value |");
    let _ = writeln!(s, "|---|---|");
    let _ = writeln!(s, "| Words | {} |", r.words);
    let _ = writeln!(s, "| Characters | {} |", r.characters);
    let _ = writeln!(s, "| Sentences | {} |", r.sentences);
    let _ = writeln!(s, "| Paragraphs | {} |", r.paragraphs);
    let _ = writeln!(s, "| Avg sentence | {:.1} words |", r.avg_sentence_words);
    let _ = writeln!(s, "| Reading time | ~{:.1} min |", r.reading_minutes);
    if let Some(f) = r.readability.flesch {
        let _ = writeln!(s, "| Flesch ease | {:.0} ({}) |", f, metrics::flesch_label(f));
    }
    if let Some((grade, label)) = metrics::grade_band(&r.readability) {
        let _ = writeln!(s, "| Grade level | ~{:.1} ({}) |", grade, label);
    }
    if !r.finding_counts.is_empty() {
        let _ = writeln!(s, "\n## Issues\n");
        for (k, n) in &r.finding_counts {
            let _ = writeln!(s, "- {k:?}: {n}");
        }
    }
    if !r.longest_sentences.is_empty() {
        let _ = writeln!(s, "\n## Longest sentences\n");
        for sref in &r.longest_sentences {
            let _ = writeln!(s, "- {} words: “{}…”", sref.words, truncate(&sref.snippet, 60));
        }
    }
    if !r.top_words.is_empty() {
        let _ = writeln!(s, "\n## Top words\n");
        let _ = writeln!(s, "{}", r.top_words.iter().map(|w| format!("{} ({})", w.word, w.count)).collect::<Vec<_>>().join(" · "));
    }
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_a_valid_report() {
        let r = build(&[], &[]);
        assert_eq!(r.words, 0);
        assert!(r.readability.flesch.is_none());
        assert!(r.findings.is_empty());
        let _ = to_text(&r);
        let _ = to_markdown(&r);
    }

    #[test]
    fn counts_and_top_words() {
        let paras = vec![ParaText { path: vec![0], text: "The quick brown fox jumps over the lazy dog. The dog barks; the fox runs.".into() }];
        let r = build(&paras, &[]);
        assert_eq!(r.sentences, 2);
        assert!(r.top_words.iter().any(|w| w.word == "fox"));
        assert_eq!(r.histogram.iter().map(|b| b.count).sum::<usize>(), r.sentences);
    }

    #[test]
    fn extra_paragraphs_add_counts_not_findings() {
        let body = vec![ParaText { path: vec![0], text: "Plain text here.".into() }];
        let extra = vec![ParaText { path: vec![0], text: "The decision was made by the committee.".into() }];
        let r = build(&body, &extra);
        assert!(r.findings.is_empty(), "findings stay body-only");
        assert!(r.words > body[0].text.split_whitespace().count());
    }
}
