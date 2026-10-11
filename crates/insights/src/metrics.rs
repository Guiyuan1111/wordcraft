//! Readability formulas: the six classic grade-level/effort metrics, computed from
//! [`stats::TextStats`] counts only. Each returns `None` when its denominator is zero, so an
//! empty or one-word document yields a report instead of a division by zero.
//!
//! For CJK-dominant documents the Latin formulas have little meaning (no syllable structure);
//! the report therefore also carries CJK-specific figures (average sentence characters) and the
//! UI hides formula columns that don't apply to the dominant script.

use crate::stats::TextStats;

/// Every readability score of one text, plus the aggregate grade label.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Readability {
    /// Flesch Reading Ease, 0–100 (higher = easier).
    pub flesch: Option<f32>,
    /// Flesch-Kincaid Grade Level (US school grade).
    pub flesch_kincaid: Option<f32>,
    /// Gunning Fog index (years of education).
    pub gunning_fog: Option<f32>,
    /// SMOG index (years of education), full-text approximation.
    pub smog: Option<f32>,
    /// Coleman-Liau index (years of education; character-based).
    pub coleman_liau: Option<f32>,
    /// Automated Readability Index (years of education; character-based).
    pub ari: Option<f32>,
}

/// Ratio of tokens that are CJK: above [`CJK_DOMINANT`] the document reads as Chinese/Japanese,
/// below [`LATIN_DOMINANT`] as Latin script, between as mixed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Script {
    #[default]
    Latin,
    Mixed,
    Cjk,
}

const CJK_DOMINANT: f32 = 0.7;
const LATIN_DOMINANT: f32 = 0.3;

pub fn dominant_script(stats: &TextStats) -> Script {
    let total = stats.tokens.len();
    if total == 0 {
        return Script::Latin;
    }
    let cjk = stats.tokens.iter().filter(|t| t.cjk).count() as f32 / total as f32;
    if cjk >= CJK_DOMINANT {
        Script::Cjk
    } else if cjk <= LATIN_DOMINANT {
        Script::Latin
    } else {
        Script::Mixed
    }
}

pub fn evaluate(stats: &TextStats) -> Readability {
    let words = stats.word_count() as f32;
    let sentences = stats.sentences.len() as f32;
    if words < 1.0 || sentences < 1.0 {
        return Readability::default();
    }
    let sents_per_word = sentences / words;
    let words_per_sent = words / sentences;
    let syll_per_word = stats.syllables as f32 / words;
    let letters_per_100 = if words > 0.0 { stats.letters as f32 / words * 100.0 } else { 0.0 };
    let sents_per_100 = sents_per_word * 100.0;
    let latin = stats.latin_words() as f32;
    // The Latin formulas need enough Latin words to say anything; a mostly-CJK document keeps
    // them `None` rather than showing nonsense grades.
    let latin_ok = latin >= 10.0 && latin / words >= 0.25;

    let flesch = latin_ok.then_some(206.835 - 1.015 * words_per_sent - 84.6 * syll_per_word);
    let flesch_kincaid = latin_ok.then_some(0.39 * words_per_sent + 11.8 * syll_per_word - 15.59);
    let gunning_fog = latin_ok.then(|| 0.4 * (words_per_sent + 100.0 * stats.complex_words as f32 / words));
    let smog = latin_ok
        .then(|| 1.0430 * (stats.polysyllables as f32 * 30.0 / sentences).sqrt() + 3.1291);
    let coleman_liau = latin_ok.then_some(0.0588 * letters_per_100 - 0.296 * sents_per_100 - 15.8);
    let ari = latin_ok.then(|| 4.71 * (stats.letters as f32 / words) + 0.5 * words_per_sent - 21.43);

    Readability { flesch, flesch_kincaid, gunning_fog, smog, coleman_liau, ari }
}

/// Plain-language band for a Flesch Reading Ease score.
pub fn flesch_label(score: f32) -> &'static str {
    match score {
        s if s >= 90.0 => "Very easy",
        s if s >= 80.0 => "Easy",
        s if s >= 70.0 => "Fairly easy",
        s if s >= 60.0 => "Plain English",
        s if s >= 50.0 => "Fairly difficult",
        s if s >= 30.0 => "Difficult",
        _ => "Very confusing",
    }
}

/// The school-grade band a document roughly needs, from an average of the grade-style metrics
/// that are present. `None` when the document has no grade metrics (too short or CJK-only).
pub fn grade_band(r: &Readability) -> Option<(f32, &'static str)> {
    let mut grades: Vec<f32> = Vec::new();
    for g in [r.flesch_kincaid, r.gunning_fog, r.smog, r.coleman_liau, r.ari].into_iter().flatten() {
        if g.is_finite() {
            grades.push(g);
        }
    }
    if grades.is_empty() {
        return None;
    }
    let avg = grades.iter().sum::<f32>() / grades.len() as f32;
    let label = match avg {
        g if g <= 6.0 => "Grade 6 and below · general audience",
        g if g <= 9.0 => "Grade 7–9 · mainstream",
        g if g <= 12.0 => "Grade 10–12 · high school",
        g if g <= 16.0 => "College",
        _ => "College+ · academic",
    };
    Some((avg, label))
}

/// Average sentence length as tokens; CJK sentences also get an average of characters, the
/// length measure Chinese style guides use (a 60-character sentence reads like a 35-word one).
pub fn averages(stats: &TextStats) -> (f32, f32) {
    let n = stats.sentences.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let words = stats.word_count() as f32 / n as f32;
    let chars = stats.sentences.iter().map(|s| s.chars).sum::<usize>() as f32 / n as f32;
    (words, chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_or_empty_is_none() {
        let r = evaluate(&TextStats::compute(""));
        assert_eq!(r, Readability::default());
    }

    #[test]
    fn simple_english_reads_easy() {
        let simple = TextStats::compute(
            "The cat sat on the mat. It saw a bird. The bird flew away. The cat ran after it. \
             The bird was gone. The cat went back to the mat. It lay down in the sun. The sun \
             was warm. The cat fell fast asleep.",
        );
        let r = evaluate(&simple);
        let f = r.flesch.unwrap_or(0.0);
        assert!(f > 70.0, "simple text should read easy, got {f}");
    }

    #[test]
    fn cjk_text_gets_no_latin_grades() {
        let r = evaluate(&TextStats::compute("这是一个测试。句子很短。可读性分析对中文文档不套用拉丁公式。"));
        assert_eq!(r.flesch, None);
    }
}
