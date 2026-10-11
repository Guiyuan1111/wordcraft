//! WordCraft insights: readability and writing-quality analysis over a [`Document`].
//!
//! The module answers "how readable is my document, and where can it improve?" without any
//! network or language model: everything is deterministic text statistics.
//!
//! - [`stats`]: sentence and word segmentation for mixed Latin/CJK text, syllable counting.
//! - [`metrics`]: the classic readability formulas (Flesch, Flesch-Kincaid, Gunning Fog, SMOG,
//!   Coleman-Liau, ARI) computed from those counts.
//! - [`detectors`]: local writing-issue checks (long sentences, passive voice, wordy phrases,
//!   filler adverbs, repeated words and sentence openers), each with a byte range so callers can
//!   jump to the text.
//! - [`report`]: the [`InsightsReport`] model (serde-ready for the engine/CLI/MCP) plus plain
//!   text and Markdown renderers.
//!
//! The entry points are [`analyze_text`] (raw text) and [`analyze_document`] (walks body blocks,
//! tables included; optionally adds the counted note/text-box stories). Analysis never fails:
//! a hostile input just yields an empty report, following the crate-wide no-panic discipline.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod detectors;
pub mod metrics;
pub mod report;
pub mod stats;

pub use report::{Headline, InsightsReport};

use wordcraft_doc::{Block, Blocks, Document, StoryRef};

/// Options for [`analyze_document`].
#[derive(Clone, Copy, Debug, Default)]
pub struct InsightsOptions {
    /// Also count footnotes, endnotes and text boxes (the stories "Word Count" counts). Findings
    /// and longest-sentence references stay body-only: only the body has stable block paths.
    pub include_notes: bool,
}

/// Analyze raw text (no document). Paragraphs are separated by `\n`.
pub fn analyze_text(text: &str) -> report::InsightsReport {
    let paras: Vec<report::ParaText> = text
        .split('\n')
        .enumerate()
        .map(|(i, t)| report::ParaText { path: vec![i as u32], text: t.to_string() })
        .collect();
    report::build(&paras, &[])
}

/// Analyze a whole document: body blocks (tables recursed into cells), optionally the counted
/// note and text-box stories for the counts.
pub fn analyze_document(doc: &Document, opts: &InsightsOptions) -> report::InsightsReport {
    let body = collect_blocks(&doc.body);
    let mut extra: Vec<report::ParaText> = Vec::new();
    if opts.include_notes {
        for story in doc.counted_stories() {
            if story == StoryRef::Body {
                continue;
            }
            if let Some(blocks) = doc.story(story) {
                extra.extend(collect_blocks(blocks));
            }
        }
    }
    report::build(&body, &extra)
}

/// Paragraphs of a block list with their block paths (table cells recurse: `[block, row, cell, …]`).
fn collect_blocks(blocks: &Blocks) -> Vec<report::ParaText> {
    let mut out = Vec::new();
    walk_blocks(blocks, &mut Vec::new(), &mut out);
    out
}

fn walk_blocks(blocks: &Blocks, prefix: &mut Vec<u32>, out: &mut Vec<report::ParaText>) {
    // The same 16-level cap the document model uses for defensive walks.
    if prefix.len() > 16 {
        return;
    }
    for (i, b) in blocks.iter().enumerate() {
        prefix.push(i as u32);
        match &**b {
            Block::Para(p) => out.push(report::ParaText { path: prefix.clone(), text: p.text.clone() }),
            Block::Table(t) => {
                for (r, row) in t.rows.iter().enumerate() {
                    prefix.push(r as u32);
                    for (c, cell) in row.cells.iter().enumerate() {
                        prefix.push(c as u32);
                        walk_blocks(&cell.blocks, prefix, out);
                        prefix.pop();
                    }
                    prefix.pop();
                }
            }
        }
        prefix.pop();
    }
}

#[cfg(test)]
mod tests;
