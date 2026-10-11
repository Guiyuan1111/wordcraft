//! Writing insights: readability and writing-quality analysis, beyond Word's Word Count.
//!
//! One pure command, `review.insights`, runs `wordcraft_insights` over the document and returns
//! the full report as JSON. Findings carry body block paths and byte offsets, so callers (the
//! UI pane, MCP agents) can jump straight to the flagged text.

use serde_json::{Value, json};

use crate::{CmdError, CmdResult, CommandSpec, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec::new("review.insights", "Writing Insights", "Review › Proofing", insights)
        .params(r#"{"includeTextBoxes"?: bool}"#)
        .pure()]
}

fn insights(s: &mut Session, v: &Value) -> CmdResult {
    let include_notes = v.get("includeTextBoxes").and_then(Value::as_bool).unwrap_or(s.prefs.count_notes);
    let opts = wordcraft_insights::InsightsOptions { include_notes };
    let report = wordcraft_insights::analyze_document(&s.doc, &opts);
    serde_json::to_value(&report).map(|mut r| {
        // Engine-style positions for every finding, ready for caret.moveTo-style navigation.
        if let Some(findings) = r.get_mut("findings").and_then(Value::as_array_mut) {
            for f in findings {
                if let (Some(path), Some(off)) = (
                    f.get("path").cloned(),
                    f.get("start").and_then(Value::as_u64),
                ) {
                    f["pos"] = json!({"story": "body", "path": path, "off": off});
                }
            }
        }
        r
    })
    .map_err(|e| CmdError::Failed(format!("insights: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insights_report_shape() {
        let mut s = Session::new(wordcraft_doc::Document::new());
        s.run("text.insert", &json!({"text": "Due to the fact that it rained, the match was cancelled."}))
            .map_err(|e| format!("{e}"))
            .unwrap_or_default();
        let r = s.run("review.insights", &json!({})).unwrap_or(Value::Null);
        assert!(r.get("words").is_some_and(|w| w.as_u64() == Some(11)), "got {r}");
        assert!(r.get("findings").and_then(Value::as_array).is_some_and(|a| !a.is_empty()));
        assert!(r["findings"][0].get("pos").is_some());
    }
}
