//! Integration tests: documents (real and hostile) through [`analyze_document`].

use wordcraft_doc::{para_block, Document, Paragraph};

use super::*;

fn doc_with(text: &str) -> Document {
    let mut doc = Document::new();
    doc.body = vec![para_block(Paragraph::with_text(text, Default::default()))];
    doc
}

#[test]
fn english_document_reports_readability() {
    let doc = doc_with(
        "The quick brown fox jumps over the lazy dog. The dog barks; the fox runs away quickly. \
         Sheep frighten the fox. The end.",
    );
    let r = analyze_document(&doc, &InsightsOptions::default());
    assert_eq!(r.paragraphs, 1);
    assert!(r.words >= 20);
    assert!(r.sentences >= 3);
    assert!(r.readability.flesch.is_some());
    assert_eq!(r.script, metrics::Script::Latin);
}

#[test]
fn cjk_document_uses_character_measures() {
    let doc = doc_with("这是一个测试文档。句子很短，可读性分析不会套用拉丁公式。平均句长按字计算。");
    let r = analyze_document(&doc, &InsightsOptions::default());
    assert_eq!(r.script, metrics::Script::Cjk);
    assert!(r.readability.flesch.is_none());
    assert!(r.avg_sentence_chars > 0.0);
}

#[test]
fn findings_carry_body_paths() {
    let doc = doc_with("Due to the fact that it rained, the picnic was cancelled by the organizers.");
    let r = analyze_document(&doc, &InsightsOptions::default());
    assert!(r.findings.iter().any(|f| f.kind == detectors::FindingKind::WordyPhrase && f.path == vec![0]));
    assert!(r.findings.iter().any(|f| f.kind == detectors::FindingKind::PassiveVoice));
}

#[test]
fn tables_recurse_into_cells() {
    let mut doc = Document::new();
    let mut cell = wordcraft_doc::table::Cell::empty();
    cell.blocks = vec![para_block(Paragraph::with_text("Cell text with a really long and winding sentence that goes on and on and on, well past the comfortable length for any reader anywhere.", Default::default()))];
    let table = wordcraft_doc::Table {
        props: Default::default(),
        grid: vec![100.0],
        rows: vec![wordcraft_doc::Row { props: Default::default(), cells: vec![cell] }],
    };
    doc.body = vec![
        para_block(Paragraph::with_text("Outside.", Default::default())),
        std::sync::Arc::new(wordcraft_doc::Block::Table(table)),
    ];
    let r = analyze_document(&doc, &InsightsOptions::default());
    assert_eq!(r.paragraphs, 2);
    assert_eq!(r.tables, 1);
    // The cell's finding carries the cell paragraph path [block=1, row=0, cell=0, para=0],
    // the same shape `Document`'s own walk produces.
    assert!(r.findings.iter().any(|f| f.path == vec![1, 0, 0, 0]));
}

#[test]
fn hostile_documents_never_panic() {
    for text in [
        "",
        "\u{FFFC}\u{FFFC}",
        "!",
        "a.e.b.c.d",
        "Dr.",
        "……",
        "!!!???。",
        &"word ".repeat(500),
        &"测".repeat(3000),
        "e.g. i.e. etc. Mr. Dr. 3.14 J.K. U.S.A.",
    ] {
        let doc = doc_with(text);
        let r = analyze_document(&doc, &InsightsOptions { include_notes: true });
        let _ = report::to_text(&r);
        let _ = report::to_markdown(&r);
    }
}

#[test]
fn json_round_trip() {
    let doc = doc_with("The report was written by the team. It is very long indeed.");
    let r = analyze_document(&doc, &InsightsOptions::default());
    let v = serde_json::to_value(&r).unwrap_or_default();
    assert_eq!(v.get("words").and_then(|w| w.as_u64()), Some(r.words as u64));
    assert!(v.get("readability").is_some());
    assert!(v.get("findings").and_then(|f| f.as_array()).is_some_and(|a| !a.is_empty()));
}
