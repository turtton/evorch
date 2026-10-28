//! Turns a tool result into the shape the program sees: `{ ok, text, truncated,
//! next, detail }` (ADR 0039, "コードに返す結果の形").
//!
//! Tool output is written for models: `read` appends a "Continue with read(...)"
//! note and shell prefixes `exit_code:`. A program must not have to parse those, so
//! they are removed here and replaced by structured fields. The text the model gets
//! from a normal tool call is unchanged; this only runs for calls made inside
//! `run_js`.

use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};

use super::diff_summary::{self, Summary, Writer};
use crate::result::ToolResult;

/// Largest output a program gets whole. Executor limits keep long output out of the
/// model's context; a program's context is its own memory, so it may have more.
/// Anything larger is returned as its tail with `next` pointing at the artifact.
pub const INLINE_OUTPUT_BYTES: u64 = 1024 * 1024;

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

/// Normalises the result of `tool` called with `args`. Output the executor moved
/// into an artifact is not restored; see [`normalize_full`].
pub fn normalize(tool: &str, args: &Value, result: &ToolResult) -> Normalized {
    normalize_with(tool, args, result, None)
}

/// [`normalize`], but output that the executor cut down to a preview and saved as a
/// complete artifact of at most [`INLINE_OUTPUT_BYTES`] is read back, so the program
/// sees all of it (`truncated` false).
pub async fn normalize_full(tool: &str, args: &Value, result: &ToolResult) -> Normalized {
    let full = full_output(result).await;
    normalize_with(tool, args, result, full)
}

/// The text of the complete artifact behind `result`, when it is small enough.
async fn full_output(result: &ToolResult) -> Option<String> {
    let artifact = result.detail.as_ref()?.get("output_artifact")?;
    if artifact.get("complete")?.as_bool() != Some(true) {
        return None;
    }
    let saved = artifact.get("saved_bytes")?.as_u64()?;
    if saved > INLINE_OUTPUT_BYTES {
        return None;
    }
    let path = std::path::PathBuf::from(artifact.get("path")?.as_str()?);
    tokio::task::spawn_blocking(move || read_artifact(&path, saved))
        .await
        .ok()?
}

/// Reads an artifact written by the executor: only under its output directory, and
/// only if it still holds exactly the `bytes` that were saved.
fn read_artifact(path: &Path, bytes: u64) -> Option<String> {
    let root = crate::output::output_root().ok()?;
    if !path.starts_with(&root) || path.components().any(|part| part.as_os_str() == "..") {
        return None;
    }
    let mut text = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(INLINE_OUTPUT_BYTES + 1)
        .read_to_end(&mut text)
        .ok()?;
    if text.len() as u64 != bytes {
        return None;
    }
    String::from_utf8(text).ok()
}

fn normalize_with(
    tool: &str,
    args: &Value,
    result: &ToolResult,
    full: Option<String>,
) -> Normalized {
    let mut text = result.content.clone();
    let mut detail = result.detail.clone().unwrap_or(Value::Null);
    let mut truncated = false;
    let mut next = None;

    let mut exit_header = None;
    if tool == "shell" {
        exit_header = split_shell_header(&mut text);
    }

    if let Some(artifact) = detail
        .get("output_artifact")
        .and_then(|artifact| artifact.get("path"))
        .and_then(Value::as_str)
        && let Some(index) = text.rfind(ARTIFACT_NOTE)
    {
        text.truncate(index);
        if let Some(full) = full {
            text = full;
            if let Some(object) = detail.as_object_mut() {
                object.remove("output_artifact");
            }
        } else {
            next = Some(json!({"tool": "read", "path": artifact}));
            truncated = true;
        }
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

    /// A shell result the executor turned into an artifact: `lines` lines of text.
    fn large_shell_result(lines: usize) -> (ToolResult, String) {
        let body: String = (0..lines).map(|index| format!("line {index}\n")).collect();
        let mut capture = crate::output::Capture::default();
        capture.push(body.as_bytes());
        let mut result = capture.finish();
        result.content = format!("exit_code: 0\n{}", result.content);
        (result, body)
    }

    #[tokio::test]
    async fn output_cut_into_an_artifact_is_returned_whole_to_the_program() {
        let (result, body) = large_shell_result(2000);
        assert!(result.content.contains("[Output artifact: "));
        let preview = normalize("shell", &json!({}), &result);
        assert!(preview.truncated, "the model-side preview is partial");
        assert!(preview.text.len() < body.len());

        let full = normalize_full("shell", &json!({}), &result).await;
        assert!(full.ok);
        assert_eq!(full.text, body);
        assert!(!full.truncated);
        assert_eq!(full.next, None);
        assert_eq!(full.detail["exit_code"], 0);
    }

    #[tokio::test]
    async fn output_too_large_to_inline_keeps_the_artifact_continuation() {
        let line = "x".repeat(1023);
        let body = format!("{line}\n").repeat(1100);
        let mut capture = crate::output::Capture::default();
        capture.push(body.as_bytes());
        let mut result = capture.finish();
        result.content = format!("exit_code: 0\n{}", result.content);
        assert!(body.len() as u64 > INLINE_OUTPUT_BYTES);

        let normalized = normalize_full("shell", &json!({}), &result).await;
        assert!(normalized.truncated);
        let path = normalized.next.as_ref().expect("a continuation")["path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
        assert!(normalized.text.len() < body.len());
    }

    #[tokio::test]
    async fn only_files_under_the_output_directory_are_read_back() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("secret.txt");
        std::fs::write(&outside, "secret").unwrap();
        let result = ToolResult::success(format!(
            "preview\n[Output artifact: {}; captured 6 of 6 bytes; complete; redacted spans: 0. Use read with offset/limit or grep. Temporary logs can expire.]",
            outside.display()
        ))
        .with_detail(json!({
            "output_artifact": {"path": outside, "original_bytes": 6, "saved_bytes": 6, "complete": true, "redactions": 0}
        }));
        let normalized = normalize_full("git_diff", &json!({}), &result).await;
        assert_eq!(normalized.text, "preview");
        assert!(normalized.truncated);
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
