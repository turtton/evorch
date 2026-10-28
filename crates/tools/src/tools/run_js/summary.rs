//! The result the model receives: printed output, the returned value and one
//! summary line, plus the `detail` the interface reads.
//!
//! Shape of `content`, which the GUI reads through `detail.run_js.layout` (byte
//! offsets of the parts below, so a printed or returned text that itself contains
//! `=> ` lines cannot be mistaken for the structure; text without a matching layout
//! falls back to reading the last `[run_js:` and `=> ` lines):
//!
//! ```text
//! N of M tool calls failed: …  (only when a call failed)
//! <printed output>
//! => <returned value>          (a string as is, anything else as JSON)
//! <error message and stack>    (only when the program failed)
//! [run_js: N calls, M failed (…); wrote: …; may have changed: …; warnings: …]
//! ```
//!
//! The last line is always the summary. It is built from what the host reported
//! for each call, so the model learns about failures, writes and checker warnings
//! even when the program discarded the call results.

use serde_json::{Value, json};

use super::serve::{Ending, Failure, Run};
use crate::result::ToolResult;

/// Detail larger than the executor's 16 KiB ceiling would be dropped entirely.
const DETAIL_BUDGET_BYTES: usize = 12 * 1024;
const REASON_CHARS: usize = 100;
const LISTED_FAILURES: usize = 5;
const LISTED_WARNINGS: usize = 3;

/// What the model is told when the time limit ended the call. The limit and how to
/// raise it are in it, and where long work belongs, so the next attempt does not
/// repeat the same call.
fn timeout_message(timeout_ms: u64, limits: &config::RunJsConfig) -> String {
    format!(
        "run_js timed out after {timeout_ms} ms and was stopped, together with the commands it had started. \
The limit is {} ms unless timeout_ms is given (at most {}). Builds, tests and other long work belong in normal shell calls; \
run them there, with the output going to a file if run_js should read it afterwards",
        limits.default_timeout_ms, limits.max_timeout_ms
    )
}

pub(super) fn finish(
    mut state: Run,
    ending: Ending,
    stderr: &str,
    timeout_ms: u64,
    limits: &config::RunJsConfig,
) -> ToolResult {
    state.calls.sort_by_key(|call| call.seq);
    state.failures.sort_by_key(|failure| failure.seq);
    let mut content = failed_calls_notice(&state)
        .map(|notice| format!("{notice}\n"))
        .unwrap_or_default();
    let output = std::mem::take(&mut state.output);
    let output_bytes = output.len();
    content.push_str(&output);
    if !output.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if state.output_truncated {
        content.push_str("[run_js: output was cut at 8 MiB]\n");
    }
    let console_end = content.len();
    let mut returned = None;
    let mut timed_out = false;
    let mut return_bytes = 0;
    let mut failure = None;
    match ending {
        Ending::Done {
            value,
            error,
            timed_out: expired,
        } => {
            timed_out = expired;
            if expired {
                state.limit_hit = Some("timeout");
            }
            if let Some(error) = error {
                let lowered = error.message.to_lowercase();
                if state.limit_hit.is_none() && lowered.contains("out of memory") {
                    state.limit_hit = Some("memory");
                } else if state.limit_hit.is_none() && lowered.contains("output limit") {
                    state.limit_hit = Some("output");
                }
                // The engine's own timeout message is replaced by the same, fuller one.
                let message = if expired {
                    timeout_message(timeout_ms, limits)
                } else {
                    error.message
                };
                failure = Some(if error.stack.trim().is_empty() {
                    message
                } else {
                    format!("{message}\n{}", error.stack.trim_end())
                });
            } else if timed_out {
                failure = Some(timeout_message(timeout_ms, limits));
            } else if !value.is_null() {
                let text = match &value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                return_bytes = text.len();
                content.push_str("=> ");
                returned = Some((content.len(), content.len() + text.len()));
                content.push_str(&text);
                content.push('\n');
            }
        }
        Ending::Failed(reason) => {
            let stderr = if stderr.is_empty() {
                String::new()
            } else {
                format!(" (worker stderr: {stderr})")
            };
            failure = Some(format!("{reason}{stderr}"));
        }
    }
    if let Some(error) = &state.drain_error {
        let note = format!(
            "run_js could not confirm that the commands it started have stopped ({error}); \
treat their effects as unknown"
        );
        failure = Some(match failure {
            Some(existing) => format!("{existing}\n{note}"),
            None => note,
        });
    }
    if let Some(failure) = &failure {
        content.push_str(failure);
        content.push('\n');
    }
    let summary_start = content.len();
    content.push_str(&summary_line(&state));
    let layout = json!({
        "bytes": content.len(),
        "console_end": console_end,
        "returned": returned.map(|(start, end)| [start, end]),
        "summary_start": summary_start,
    });
    let detail =
        json!({"run_js": run_detail(&state, output_bytes, return_bytes, timed_out, layout)});
    let result = if failure.is_some() {
        ToolResult::error(content)
    } else {
        ToolResult::success(content)
    };
    result.with_detail(detail)
}

/// The first line of the result when a call failed. A failed call does not make the
/// `run_js` call an error (the program may have handled it), so it is stated where
/// the model reads first instead of only in the summary line at the end. The GUI
/// reads the last line and the last `=> ` line, so a leading line does not affect it.
fn failed_calls_notice(state: &Run) -> Option<String> {
    let failed = state.calls.iter().filter(|call| call.is_error).count();
    if failed == 0 {
        return None;
    }
    let reasons = if state.failures.is_empty() {
        String::new()
    } else {
        format!(": {}", failure_list(&state.failures))
    };
    Some(format!(
        "{failed} of {} tool calls failed{reasons}",
        state.calls.len()
    ))
}

/// One line, however much went wrong: reasons are cut and listed up to a limit.
fn summary_line(state: &Run) -> String {
    let failed = state.calls.iter().filter(|call| call.is_error).count();
    let mut line = format!("[run_js: {} calls, {failed} failed", state.calls.len());
    if !state.failures.is_empty() {
        line.push_str(&format!(" ({})", failure_list(&state.failures)));
    }
    if !state.written.is_empty() {
        line.push_str(&format!("; wrote: {}", join_paths(&state.written)));
    }
    if !state.uncertain.is_empty() {
        line.push_str(&format!(
            "; may have changed: {}",
            join_paths(&state.uncertain)
        ));
    }
    if !state.warnings.is_empty() {
        let shown: Vec<String> = state
            .warnings
            .iter()
            .take(LISTED_WARNINGS)
            .map(|(seq, name, text)| format!("{name} #{seq}: {}", one_line(text, REASON_CHARS)))
            .collect();
        let more = state.warnings.len().saturating_sub(LISTED_WARNINGS);
        let more = if more > 0 {
            format!("; +{more} more")
        } else {
            String::new()
        };
        line.push_str(&format!("; warnings: {}{more}", shown.join("; ")));
    }
    line.push(']');
    line
}

fn failure_list(failures: &[Failure]) -> String {
    let mut parts: Vec<String> = failures
        .iter()
        .take(LISTED_FAILURES)
        .map(|failure| {
            format!(
                "{} #{}: {}",
                failure.name,
                failure.seq,
                one_line(&failure.reason, REASON_CHARS)
            )
        })
        .collect();
    if failures.len() > LISTED_FAILURES {
        parts.push(format!("+{} more", failures.len() - LISTED_FAILURES));
    }
    parts.join("; ")
}

fn join_paths(paths: &std::collections::BTreeSet<String>) -> String {
    paths
        .iter()
        .map(|path| one_line(path, 200))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `text` on one line, control characters flattened and the length bounded.
pub(super) fn one_line(text: &str, limit: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut: String = flat.chars().take(limit).collect();
    cut.push('…');
    cut
}

fn run_detail(
    state: &Run,
    output_bytes: usize,
    return_bytes: usize,
    timed_out: bool,
    layout: Value,
) -> Value {
    let written: Vec<&String> = state.written.iter().collect();
    let uncertain: Vec<&String> = state.uncertain.iter().collect();
    let calls_value = |keep_paths: bool, limit: usize| -> Vec<Value> {
        state
            .calls
            .iter()
            .take(limit)
            .map(|call| {
                let paths = if keep_paths {
                    call.paths.clone()
                } else {
                    Vec::new()
                };
                json!({"seq": call.seq, "name": call.name, "is_error": call.is_error, "paths": paths})
            })
            .collect()
    };
    let build = |calls: Vec<Value>| {
        json!({
            "calls": calls,
            "calls_omitted": state.calls.len().saturating_sub(calls.len()),
            "calls_failed": state.calls.iter().filter(|call| call.is_error).count(),
            "output_bytes": output_bytes,
            "return_bytes": return_bytes,
            "layout": layout,
            "timed_out": timed_out,
            "limit_hit": state.limit_hit,
            "written_paths": written,
            "uncertain_paths": uncertain,
        })
    };
    let full = build(calls_value(true, usize::MAX));
    if full.to_string().len() <= DETAIL_BUDGET_BYTES {
        return full;
    }
    let without_paths = build(calls_value(false, usize::MAX));
    if without_paths.to_string().len() <= DETAIL_BUDGET_BYTES {
        return without_paths;
    }
    let mut keep = state.calls.len();
    loop {
        keep = keep * 3 / 4;
        let trimmed = build(calls_value(false, keep));
        if keep == 0 || trimmed.to_string().len() <= DETAIL_BUDGET_BYTES {
            return trimmed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::run_js::protocol::JsError;
    use crate::tools::run_js::serve::Recorded;

    fn done(value: Value) -> Ending {
        Ending::Done {
            value,
            error: None,
            timed_out: false,
        }
    }

    fn failed_call(seq: u32, name: &str, reason: &str) -> (Recorded, Failure) {
        (
            Recorded {
                seq,
                name: name.into(),
                is_error: true,
                paths: Vec::new(),
            },
            Failure {
                seq,
                name: name.into(),
                reason: reason.into(),
            },
        )
    }

    #[test]
    fn a_string_return_value_is_shown_as_is() {
        let result = finish(
            Run::default(),
            done(json!("line one\nline two")),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert_eq!(
            result.content,
            "=> line one\nline two\n[run_js: 0 calls, 0 failed]"
        );
        let result = finish(
            Run::default(),
            done(json!({"a": [1, "x"]})),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert_eq!(
            result.content,
            "=> {\"a\":[1,\"x\"]}\n[run_js: 0 calls, 0 failed]"
        );
    }

    #[test]
    fn the_summary_names_failed_calls_even_when_the_program_returns_nothing() {
        let mut state = Run::default();
        for (call, failure) in [
            failed_call(1, "edit", "old_string not found\nin /w/a.rs"),
            failed_call(3, "read", "no such file"),
        ] {
            state.calls.push(call);
            state.failures.push(failure);
        }
        state.written.insert("/w/b.rs".into());
        state
            .warnings
            .push((2, "write".into(), "comment checker flagged /w/b.rs".into()));
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert_eq!(
            result.content,
            "2 of 2 tool calls failed: edit #1: old_string not found in /w/a.rs; read #3: no such file\n\
[run_js: 2 calls, 2 failed (edit #1: old_string not found in /w/a.rs; read #3: no such file); \
wrote: /w/b.rs; warnings: write #2: comment checker flagged /w/b.rs]"
        );
        assert!(
            !result.is_error,
            "failed calls alone do not fail the program"
        );
    }

    #[test]
    fn the_failure_notice_comes_first_and_leaves_the_last_lines_to_the_interface() {
        let mut state = Run {
            output: "progress\n".into(),
            ..Run::default()
        };
        for seq in 1..=3 {
            state.calls.push(Recorded {
                seq,
                name: "read".into(),
                is_error: seq == 2,
                paths: Vec::new(),
            });
        }
        let (_, failure) = failed_call(2, "edit", "old_string not found");
        state.failures.push(failure);
        let result = finish(
            state,
            done(json!("answer")),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let lines: Vec<&str> = result.content.lines().collect();
        assert_eq!(
            lines[0],
            "1 of 3 tool calls failed: edit #2: old_string not found"
        );
        assert_eq!(lines[1], "progress");
        assert_eq!(lines[2], "=> answer");
        assert!(lines[3].starts_with("[run_js: 3 calls, 1 failed"));
        assert_eq!(lines.len(), 4);
        assert_eq!(result.detail.unwrap()["run_js"]["output_bytes"], 9);
        let clean = finish(
            Run::default(),
            done(json!("ok")),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert!(!clean.content.contains("tool calls failed"));
    }

    #[test]
    fn the_layout_locates_the_parts_even_when_the_texts_contain_return_markers() {
        let mut state = Run {
            output: "step\n=> printed, not returned".into(),
            ..Run::default()
        };
        let (call, failure) = failed_call(1, "read", "no such file");
        state.calls.push(call);
        state.failures.push(failure);
        let returned = "first\n=> second line of the same string\nthird";
        let result = finish(
            state,
            done(json!(returned)),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let layout = &result.detail.as_ref().unwrap()["run_js"]["layout"];
        let content = &result.content;
        let at = |key: &str| usize::try_from(layout[key].as_u64().unwrap()).unwrap();
        assert_eq!(at("bytes"), content.len());
        assert_eq!(
            &content[..at("console_end")],
            "1 of 1 tool calls failed: read #1: no such file\nstep\n=> printed, not returned\n"
        );
        let range = layout["returned"].as_array().unwrap();
        let (start, end) = (
            usize::try_from(range[0].as_u64().unwrap()).unwrap(),
            usize::try_from(range[1].as_u64().unwrap()).unwrap(),
        );
        assert_eq!(&content[start..end], returned);
        assert!(content[at("summary_start")..].starts_with("[run_js: 1 calls, 1 failed"));

        let nothing = finish(
            Run::default(),
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let layout = &nothing.detail.unwrap()["run_js"]["layout"];
        assert!(layout["returned"].is_null());
        assert_eq!(layout["console_end"], 0);
        assert_eq!(layout["summary_start"], 0);
    }

    #[test]
    fn a_timeout_says_what_the_limit_is_and_where_long_work_belongs() {
        let limits = config::RunJsConfig::default();
        let host_side = finish(
            Run::default(),
            Ending::Done {
                value: Value::Null,
                error: None,
                timed_out: true,
            },
            "",
            30_000,
            &limits,
        );
        // The engine's own report of the same timeout gets the same words, and its stack.
        let engine_side = finish(
            Run::default(),
            Ending::Done {
                value: Value::Null,
                error: Some(JsError {
                    message: "run_js timed out after 30000 ms".into(),
                    stack: "    at <anonymous> (run_js:3:1)".into(),
                }),
                timed_out: true,
            },
            "",
            30_000,
            &limits,
        );
        for result in [&host_side, &engine_side] {
            assert!(result.is_error);
            let text = &result.content;
            assert!(
                text.contains("run_js timed out after 30000 ms and was stopped"),
                "{text}"
            );
            assert!(
                text.contains("The limit is 30000 ms unless timeout_ms is given (at most 300000)"),
                "{text}"
            );
            assert!(
                text.contains("Builds, tests and other long work belong in normal shell calls"),
                "{text}"
            );
            assert_eq!(result.detail.as_ref().unwrap()["run_js"]["timed_out"], true);
        }
        assert!(engine_side.content.contains("at <anonymous> (run_js:3:1)"));
    }

    #[test]
    fn long_failure_lists_and_reasons_are_bounded_to_one_line() {
        let mut state = Run::default();
        for seq in 1..=8 {
            let (call, failure) = failed_call(seq, "grep", &"x".repeat(500));
            state.calls.push(call);
            state.failures.push(failure);
        }
        let line = summary_line(&state);
        assert!(!line.contains('\n'));
        assert!(line.contains("+3 more"), "{line}");
        assert!(line.len() < 1200, "{} bytes", line.len());
        assert!(line.ends_with(']'));
    }

    #[test]
    fn cut_off_writes_are_reported_as_uncertain() {
        let mut state = Run::default();
        state.uncertain.insert("/w/half.rs".into());
        let (call, failure) = failed_call(1, "write", "cut off before it finished");
        state.calls.push(call);
        state.failures.push(failure);
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert!(
            result.content.contains("may have changed: /w/half.rs"),
            "{}",
            result.content
        );
        assert_eq!(
            result.detail.unwrap()["run_js"]["uncertain_paths"],
            json!(["/w/half.rs"])
        );
    }

    #[test]
    fn detail_stays_within_budget_for_many_calls() {
        let mut state = Run::default();
        for seq in 1..=200 {
            state.calls.push(Recorded {
                seq,
                name: "read".into(),
                is_error: false,
                paths: vec![format!("/workspace/some/long/path/segment/file-{seq}.rs")],
            });
        }
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let detail = result.detail.unwrap();
        assert!(detail.to_string().len() <= DETAIL_BUDGET_BYTES);
    }
}
