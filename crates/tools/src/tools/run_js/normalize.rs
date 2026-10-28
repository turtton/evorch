//! Turns a tool result into the shape the program sees: `{ ok, text, truncated,
//! next, detail }` (ADR 0039, "コードに返す結果の形").
//!
//! Tool output is written for models: `read` appends a "Continue with read(...)"
//! note and shell prefixes `exit_code:`. A program must not have to parse those, so
//! they are removed here and replaced by structured fields. The text the model gets
//! from a normal tool call is unchanged; this only runs for calls made inside
//! `run_js`.

use serde_json::{Value, json};

use super::diff_summary::{self, Summary, Writer};
#[cfg(test)]
use crate::result::FullContent;
use crate::result::ToolResult;

const READ_NOTE: &str = "\n\n[Output truncated (range limit:";
const ARTIFACT_NOTE: &str = "\n[Output artifact: ";
const UNSAVED_NOTE: &str = "\n[Output truncated;";
const GREP_NOTE: &str = "[truncated: ";

/// The result of one call as the program sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    pub ok: bool,
    pub text: String,
    pub truncated: bool,
    /// Arguments that continue the output, e.g. `{tool: "read", path, offset}`.
    pub next: Option<Value>,
    pub detail: Value,
}

impl Normalized {
    pub fn into_value(self) -> Value {
        json!({
            "ok": self.ok,
            "text": self.text,
            "truncated": self.truncated,
            "next": self.next,
            "detail": self.detail,
        })
    }

    /// A call that never produced a result (denied, invalid arguments, failure).
    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            text: message.into(),
            truncated: false,
            next: None,
            detail: Value::Null,
        }
    }
}

/// Normalises the result of `tool` called with `args`.
pub fn normalize(tool: &str, args: &Value, result: &ToolResult) -> Normalized {
    // The whole text, when the executor kept it for this call (see
    // `ToolResult::full_content`), replaces the preview and its artifact note.
    let whole = result.full_content.is_some();
    let mut text = result
        .full_content
        .as_deref()
        .map_or_else(|| result.content.clone(), |whole| whole.as_str().to_owned());
    let mut detail = result.detail.clone().unwrap_or(Value::Null);
    let mut truncated = false;
    let mut next = None;

    let mut exit_header = None;
    if tool == "shell" {
        exit_header = split_shell_header(&mut text).or_else(|| shell_status(&detail));
    }

    if whole {
        if let Some(object) = detail.as_object_mut() {
            object.remove("output_artifact");
        }
    } else if let Some(artifact) = detail
        .get("output_artifact")
        .and_then(|artifact| artifact.get("path"))
        .and_then(Value::as_str)
        && let Some(index) = text.rfind(ARTIFACT_NOTE)
    {
        next = Some(json!({"tool": "read", "path": artifact}));
        truncated = true;
        text.truncate(index);
    } else if let Some(index) = text.rfind(UNSAVED_NOTE)
        && text.ends_with(']')
    {
        truncated = true;
        text.truncate(index);
    }

    match tool {
        "read" => {
            if detail.get("truncated").and_then(Value::as_bool) == Some(true)
                && let Some(index) = text.rfind(READ_NOTE)
            {
                text.truncate(index);
                truncated = true;
                next = Some(json!({
                    "tool": "read",
                    "path": detail.get("path").or_else(|| args.get("path")).cloned().unwrap_or(Value::Null),
                    "offset": detail.get("next_offset").cloned().unwrap_or(Value::Null),
                    "byte_offset": detail.get("next_byte_offset").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        "grep" => {
            if let Some((rest, omitted, at_least)) = split_grep_note(&text) {
                text = rest;
                truncated = true;
                detail = json!({"omitted_lines": omitted, "omitted_is_lower_bound": at_least});
            }
        }
        "shell" => {
            detail = json!({
                "exit_code": exit_header.as_ref().map(|header| header.exit_code),
                "timed_out_ms": exit_header.as_ref().and_then(|header| header.timed_out_ms),
            });
        }
        "edit" | "write" | "git_diff" if !truncated && !result.is_error => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
            let summary = match tool {
                "edit" => diff_summary::summarize_file(Writer::Edit, path, &text),
                "write" => diff_summary::summarize_file(Writer::Write, path, &text),
                _ => diff_summary::summarize_git_diff(&text),
            };
            if let Some(Summary {
                text: summary,
                detail: extra,
            }) = summary
            {
                text = summary;
                merge(&mut detail, extra);
            }
        }
        _ => {}
    }

    Normalized {
        ok: !result.is_error,
        text,
        truncated,
        next,
        detail,
    }
}

/// Adds the fields of `extra` to `detail`, which is `null` or an object.
fn merge(detail: &mut Value, extra: Value) {
    if !detail.is_object() {
        *detail = json!({});
    }
    if let (Some(detail), Value::Object(extra)) = (detail.as_object_mut(), extra) {
        detail.extend(extra);
    }
}

/// The exit status a cut shell result carries in its detail, for when the ceiling
/// trimmed the header line off the preview.
fn shell_status(detail: &Value) -> Option<ShellHeader> {
    let status = detail.get("shell")?;
    Some(ShellHeader {
        exit_code: status.get("exit_code")?.as_i64()?,
        timed_out_ms: status.get("timed_out_ms").and_then(Value::as_u64),
    })
}

struct ShellHeader {
    exit_code: i64,
    timed_out_ms: Option<u64>,
}

fn split_shell_header(text: &mut String) -> Option<ShellHeader> {
    let rest = text.strip_prefix("exit_code: ")?;
    let (code, mut body) = rest.split_once('\n').unwrap_or((rest, ""));
    let exit_code = code.trim().parse().ok()?;
    let mut timed_out_ms = None;
    if let Some(after) = body.strip_prefix("timed out after ")
        && let Some((ms, remainder)) = after.split_once(" ms; partial output follows\n")
    {
        timed_out_ms = ms.parse().ok();
        body = remainder;
    }
    *text = body.to_owned();
    Some(ShellHeader {
        exit_code,
        timed_out_ms,
    })
}

fn split_grep_note(text: &str) -> Option<(String, u64, bool)> {
    let start = if text.starts_with(GREP_NOTE) {
        0
    } else {
        text.rfind(&format!("\n{GREP_NOTE}"))? + 1
    };
    let note = text[start..].trim_end();
    let body = note.strip_prefix(GREP_NOTE)?.strip_suffix(" more lines]")?;
    let (at_least, count) = match body.strip_prefix("at least ") {
        Some(count) => (true, count),
        None => (false, body),
    };
    let omitted = count.parse().ok()?;
    Some((
        text[..start].trim_end_matches('\n').to_owned(),
        omitted,
        at_least,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_args() -> Value {
        json!({"path": "/w/a.txt"})
    }

    #[test]
    fn complete_read_has_no_continuation() {
        let result = ToolResult::success("line1\nline2\n").with_detail(json!({
            "path": "/w/a.txt", "truncated": false, "next_offset": null, "next_byte_offset": null
        }));
        let normalized = normalize("read", &read_args(), &result);
        assert!(normalized.ok);
        assert_eq!(normalized.text, "line1\nline2\n");
        assert!(!normalized.truncated);
        assert_eq!(normalized.next, None);
    }

    #[test]
    fn truncated_read_drops_the_model_note_and_returns_the_next_arguments() {
        let content = "body\n\n\n[Output truncated (range limit: 300 lines / 16384 bytes). Continue with read({\"path\":\"/w/a.txt\"}).]";
        let result = ToolResult::success(content).with_detail(json!({
            "path": "/w/a.txt", "truncated": true, "next_offset": 301, "next_byte_offset": 0
        }));
        let normalized = normalize("read", &read_args(), &result);
        assert_eq!(normalized.text, "body\n");
        assert!(normalized.truncated);
        assert_eq!(
            normalized.next,
            Some(json!({"tool": "read", "path": "/w/a.txt", "offset": 301, "byte_offset": 0}))
        );
    }

    #[test]
    fn grep_truncation_line_becomes_a_flag() {
        let result = ToolResult::success("a.rs:1:x\nb.rs:2:y\n[truncated: at least 40 more lines]");
        let normalized = normalize("grep", &json!({"pattern": "x", "path": "."}), &result);
        assert_eq!(normalized.text, "a.rs:1:x\nb.rs:2:y");
        assert!(normalized.truncated);
        assert_eq!(
            normalized.detail,
            json!({"omitted_lines": 40, "omitted_is_lower_bound": true})
        );
        let exact = ToolResult::success("[truncated: 3 more lines]");
        let normalized = normalize("grep", &json!({}), &exact);
        assert_eq!(normalized.text, "");
        assert_eq!(normalized.detail["omitted_is_lower_bound"], false);
    }

    #[test]
    fn grep_without_a_note_is_untouched() {
        let result = ToolResult::success("a.rs:1:[truncated: not a note]");
        let normalized = normalize("grep", &json!({}), &result);
        assert_eq!(normalized.text, "a.rs:1:[truncated: not a note]");
        assert!(!normalized.truncated);
    }

    #[test]
    fn shell_output_and_exit_code_are_separated() {
        let result = ToolResult::error("exit_code: 7\nfailed\n");
        let normalized = normalize("shell", &json!({"command": "x"}), &result);
        assert!(!normalized.ok);
        assert_eq!(normalized.text, "failed\n");
        assert_eq!(
            normalized.detail,
            json!({"exit_code": 7, "timed_out_ms": null})
        );

        let timed_out = ToolResult::error(
            "exit_code: -1\ntimed out after 1500 ms; partial output follows\npart",
        );
        let normalized = normalize("shell", &json!({}), &timed_out);
        assert_eq!(normalized.text, "part");
        assert_eq!(normalized.detail["timed_out_ms"], 1500);
    }

    #[test]
    fn output_artifacts_become_a_read_continuation() {
        let content = "exit_code: 0\ntail of the log\n[Output artifact: /var/tmp/o/1.txt; captured 9 of 9 bytes; complete; redacted spans: 0. Use read with offset/limit or grep. Temporary logs can expire.]";
        let result = ToolResult::success(content).with_detail(json!({
            "output_artifact": {"path": "/var/tmp/o/1.txt", "original_bytes": 9}
        }));
        let normalized = normalize("shell", &json!({}), &result);
        assert_eq!(normalized.text, "tail of the log");
        assert!(normalized.truncated);
        assert_eq!(
            normalized.next,
            Some(json!({"tool": "read", "path": "/var/tmp/o/1.txt"}))
        );
        assert_eq!(normalized.detail["exit_code"], 0);
    }

    #[test]
    fn unsaved_truncation_is_flagged_without_a_continuation() {
        let result = ToolResult::success(
            "preview\n[Output truncated; 99999 bytes produced. Artifact unavailable: disk full]",
        );
        let normalized = normalize("git_diff", &json!({}), &result);
        assert_eq!(normalized.text, "preview");
        assert!(normalized.truncated);
        assert_eq!(normalized.next, None);
    }

    #[test]
    fn edit_and_write_replies_become_a_summary_with_the_diff_in_detail() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let result = ToolResult::success(diff);
        let normalized = normalize("edit", &json!({"path": "f.rs"}), &result);
        assert!(normalized.ok);
        assert_eq!(normalized.text, "edited f.rs (+1 −1)");
        assert!(!normalized.truncated);
        assert_eq!(
            normalized.detail,
            json!({"diff": diff, "added": 1, "removed": 1})
        );
        let normalized = normalize("write", &json!({"path": "f.rs"}), &result);
        assert_eq!(normalized.text, "wrote f.rs (+1 −1)");
    }

    #[test]
    fn a_checker_warning_detail_survives_the_summary() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let result = ToolResult::success(format!(
            "{diff}\n[comment-checker warning]\n> x\n[end comment-checker warning]"
        ))
        .with_detail(json!({"comment_checker": {"outcome": "warning"}}));
        let normalized = normalize("edit", &json!({"path": "f.rs"}), &result);
        assert!(
            normalized
                .text
                .starts_with("edited f.rs (+1 −1)\n[comment-checker")
        );
        assert_eq!(normalized.detail["comment_checker"]["outcome"], "warning");
        assert_eq!(normalized.detail["diff"], diff);
    }

    #[test]
    fn a_failed_edit_keeps_its_message_and_git_diff_is_summarised() {
        let failed = normalize(
            "edit",
            &json!({"path": "f.rs"}),
            &ToolResult::error("--- looks like a diff but the call failed"),
        );
        assert_eq!(failed.text, "--- looks like a diff but the call failed");
        assert_eq!(failed.detail, Value::Null);

        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let normalized = normalize("git_diff", &json!({}), &ToolResult::success(diff));
        assert_eq!(normalized.text, "1 file changed (+1 −1)\nx (+1 −1)");
        assert_eq!(normalized.detail["diff"], diff);
        let clean = normalize("git_diff", &json!({}), &ToolResult::success(""));
        assert_eq!(clean.text, "");
    }

    #[test]
    fn the_whole_text_replaces_the_preview_and_its_artifact_reference() {
        let preview = "exit_code: 0\ntail of the log\n[Output artifact: /var/tmp/o/1.txt; captured 9 of 9 bytes; complete; redacted spans: 0. Use read with offset/limit or grep. Temporary logs can expire.]";
        let mut result = ToolResult::success(preview).with_detail(json!({
            "output_artifact": {"path": "/var/tmp/o/1.txt", "original_bytes": 9}
        }));
        result.full_content = Some(FullContent::new(
            "exit_code: 0\nhead of the log\ntail of the log".into(),
        ));
        let normalized = normalize("shell", &json!({}), &result);
        assert_eq!(normalized.text, "head of the log\ntail of the log");
        assert!(!normalized.truncated);
        assert_eq!(normalized.next, None);
        assert_eq!(
            normalized.detail,
            json!({"exit_code": 0, "timed_out_ms": null})
        );
    }

    #[test]
    fn a_large_edit_diff_is_summarised_from_its_whole_text() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let mut result = ToolResult::success("[cut preview]\n[Output artifact: /var/tmp/o/2.txt; captured 1 of 1 bytes; complete; redacted spans: 0. Use read with offset/limit or grep. Temporary logs can expire.]")
            .with_detail(json!({"output_artifact": {"path": "/var/tmp/o/2.txt", "original_bytes": 1}}));
        result.full_content = Some(FullContent::new(diff.into()));
        let normalized = normalize("edit", &json!({"path": "f.rs"}), &result);
        assert_eq!(normalized.text, "edited f.rs (+1 −1)");
        assert_eq!(
            normalized.detail,
            json!({"diff": diff, "added": 1, "removed": 1})
        );
    }

    #[test]
    fn a_cut_shell_result_whose_header_was_trimmed_keeps_its_status_from_the_detail() {
        // The preview starts mid-output: no `exit_code:` line, but the detail has it.
        let content = "...end of the output\n[Output artifact: /var/tmp/o/3.txt; captured 9 of 9 bytes; complete; redacted spans: 0. Use read with offset/limit or grep. Temporary logs can expire.]";
        let result = ToolResult::error(content).with_detail(json!({
            "output_artifact": {"path": "/var/tmp/o/3.txt", "original_bytes": 9},
            "shell": {"exit_code": -1, "timed_out_ms": 30000}
        }));
        let normalized = normalize("shell", &json!({}), &result);
        assert!(!normalized.ok);
        assert_eq!(normalized.text, "...end of the output");
        assert!(normalized.truncated);
        assert_eq!(
            normalized.detail,
            json!({"exit_code": -1, "timed_out_ms": 30000})
        );
    }

    #[test]
    fn edit_errors_and_failures_keep_their_message() {
        let normalized = normalize(
            "edit",
            &json!({}),
            &ToolResult::error("old_string not found"),
        );
        assert!(!normalized.ok);
        assert_eq!(normalized.text, "old_string not found");
        let failure = Normalized::failure("denied").into_value();
        assert_eq!(
            failure,
            json!({"ok": false, "text": "denied", "truncated": false, "next": null, "detail": null})
        );
    }
}
