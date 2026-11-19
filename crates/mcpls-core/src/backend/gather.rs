//! One answer to the host from the answers of several backends to one tool call.

use serde_json::{Value, json};

/// Merges the `get_new_diagnostics` JSON-RPC responses in `parts` into one
/// response carrying `id`. The session backend's answer comes first.
///
/// A part is usable when it has a `result`, `result.isError` is not `true`,
/// and `result.content[0].text` parses as a JSON object. The merged report
/// concatenates `changed` and `cleared` in part order, sums `omitted`, and
/// joins the distinct non-empty `note` strings with a single space. It has no
/// `note` key when there are none. When no part is usable, the answer is the
/// first part with its `id` replaced by `id`.
pub fn merge(id: &serde_json::Value, parts: Vec<serde_json::Value>) -> serde_json::Value {
    let reports: Vec<Value> = parts.iter().filter_map(usable_report).collect();
    if reports.is_empty() {
        return first_answer(id, parts);
    }

    let mut changed = Vec::new();
    let mut cleared = Vec::new();
    let mut omitted = 0;
    let mut notes: Vec<&str> = Vec::new();
    for report in &reports {
        changed.extend(report["changed"].as_array().cloned().unwrap_or_default());
        cleared.extend(report["cleared"].as_array().cloned().unwrap_or_default());
        omitted += report["omitted"].as_u64().unwrap_or(0);
        let note = report["note"].as_str().unwrap_or_default();
        if !note.is_empty() && !notes.contains(&note) {
            notes.push(note);
        }
    }

    let mut report = json!({ "changed": changed, "cleared": cleared, "omitted": omitted });
    if !notes.is_empty() {
        report["note"] = json!(notes.join(" "));
    }
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": report.to_string() }],
            "isError": false
        }
    })
}

/// Merges the `workspace_symbol_search` responses in `parts` into one
/// response carrying `id`. The symbols keep part order, and the merged list
/// stops at `limit`. Parts are usable as for `merge`.
pub fn merge_symbols(id: &Value, parts: Vec<Value>, limit: usize) -> Value {
    let reports: Vec<Value> = parts.iter().filter_map(usable_report).collect();
    if reports.is_empty() {
        return first_answer(id, parts);
    }

    let mut symbols: Vec<Value> = reports
        .iter()
        .flat_map(|report| report["symbols"].as_array().cloned().unwrap_or_default())
        .collect();
    symbols.truncate(limit);
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": json!({ "symbols": symbols }).to_string() }],
            "isError": false
        }
    })
}

/// The answer when no part is usable: the first part with its `id` replaced,
/// or a "no backend answered" error when there are no parts.
fn first_answer(id: &Value, parts: Vec<Value>) -> Value {
    let Some(mut first) = parts.into_iter().next() else {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": "no backend answered" }
        });
    };
    first["id"] = id.clone();
    first
}

/// The report object in a part's `result`, when the part is usable.
fn usable_report(part: &Value) -> Option<Value> {
    let result = part.get("result")?;
    if result["isError"] == json!(true) {
        return None;
    }
    let text = result["content"][0]["text"].as_str()?;
    let report: Value = serde_json::from_str(text).ok()?;
    report.is_object().then_some(report)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Wraps `report` as a successful `get_new_diagnostics` response with id "p".
    fn answer(report: &Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": "p",
            "result": {
                "content": [{ "type": "text", "text": report.to_string() }],
                "isError": false
            }
        })
    }

    /// The merged report carried by a response's `content[0].text`.
    fn report_of(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        serde_json::from_str(text).expect("report JSON")
    }

    #[test]
    fn test_reports_from_two_backends_are_merged() {
        let first = answer(&json!({
            "changed": [{ "file_path": "/a/x.rs", "diagnostics": [], "omitted": 0 }],
            "cleared": ["/a/old.rs"],
            "omitted": 1
        }));
        let second = answer(&json!({
            "changed": [{ "file_path": "/b/y.rs", "diagnostics": [], "omitted": 0 }],
            "cleared": ["/b/old.rs"],
            "omitted": 2
        }));

        let merged = merge(&json!(7), vec![first, second]);

        assert_eq!(merged["id"], json!(7));
        assert_eq!(merged["result"]["isError"], json!(false));
        let report = report_of(&merged);
        assert_eq!(report["changed"][0]["file_path"], json!("/a/x.rs"));
        assert_eq!(report["changed"][1]["file_path"], json!("/b/y.rs"));
        assert_eq!(report["cleared"], json!(["/a/old.rs", "/b/old.rs"]));
        assert_eq!(report["omitted"], json!(3));
        assert!(report.get("note").is_none());
    }

    #[test]
    fn test_distinct_notes_are_joined_once() {
        let parts = vec![
            answer(&json!({ "changed": [], "cleared": [], "omitted": 0, "note": "n1" })),
            answer(&json!({ "changed": [], "cleared": [], "omitted": 0, "note": "n1" })),
            answer(&json!({ "changed": [], "cleared": [], "omitted": 0, "note": "n2" })),
        ];

        let merged = merge(&json!(7), parts);

        assert_eq!(report_of(&merged)["note"], json!("n1 n2"));
    }

    #[test]
    fn test_an_unusable_part_is_skipped() {
        let first = answer(&json!({ "changed": [], "cleared": ["/a/old.rs"], "omitted": 1 }));
        let failed = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "gone" }
        });

        let merged = merge(&json!(7), vec![first.clone(), failed]);

        assert_eq!(report_of(&merged), report_of(&first));
    }

    #[test]
    fn test_with_no_usable_part_the_first_part_answers() {
        let first = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "first" }
        });
        let second = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "second" }
        });

        let merged = merge(&json!(7), vec![first.clone(), second]);

        let mut expected = first;
        expected["id"] = json!(7);
        assert_eq!(merged, expected);
    }

    /// A `symbols` report holding one function named `name`.
    fn symbol(name: &str) -> Value {
        json!({
            "name": name,
            "kind": "Function",
            "location": {
                "uri": "file:///x.rs",
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 }
                }
            }
        })
    }

    /// The symbol names in a merged symbols response, in order.
    fn names_of(response: &Value) -> Vec<String> {
        report_of(response)["symbols"]
            .as_array()
            .expect("symbols array")
            .iter()
            .map(|symbol| symbol["name"].as_str().expect("name").to_owned())
            .collect()
    }

    #[test]
    fn test_symbols_from_two_backends_are_concatenated_in_part_order() {
        let first = answer(&json!({ "symbols": [symbol("a")] }));
        let second = answer(&json!({ "symbols": [symbol("b")] }));

        let merged = merge_symbols(&json!(7), vec![first, second], 100);

        assert_eq!(merged["id"], json!(7));
        assert_eq!(merged["result"]["isError"], json!(false));
        assert_eq!(names_of(&merged), ["a", "b"]);
    }

    #[test]
    fn test_an_unusable_symbols_part_is_skipped() {
        let first = answer(&json!({ "symbols": [symbol("a")] }));
        let failed = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "gone" }
        });
        let refused = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "result": {
                "content": [{ "type": "text", "text": json!({ "symbols": [symbol("c")] }).to_string() }],
                "isError": true
            }
        });

        let merged = merge_symbols(&json!(7), vec![first, failed, refused], 100);

        assert_eq!(names_of(&merged), ["a"]);
    }

    #[test]
    fn test_merged_symbols_stop_at_the_limit() {
        let first = answer(&json!({ "symbols": [symbol("a"), symbol("b")] }));
        let second = answer(&json!({ "symbols": [symbol("c")] }));

        let merged = merge_symbols(&json!(7), vec![first, second], 2);

        assert_eq!(names_of(&merged), ["a", "b"]);
    }

    #[test]
    fn test_with_no_usable_symbols_part_the_first_part_answers() {
        let first = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "first" }
        });
        let second = json!({
            "jsonrpc": "2.0",
            "id": "p",
            "error": { "code": -32603, "message": "second" }
        });

        let merged = merge_symbols(&json!(7), vec![first.clone(), second], 100);

        let mut expected = first;
        expected["id"] = json!(7);
        assert_eq!(merged, expected);
    }
}
