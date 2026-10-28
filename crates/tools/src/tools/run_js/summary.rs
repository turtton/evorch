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
use crate::result::{CutOffTold, ToolResult};

/// Detail larger than the executor's 16 KiB ceiling would be dropped entirely.
const DETAIL_BUDGET_BYTES: usize = 12 * 1024;
const REASON_CHARS: usize = 100;
const LISTED_FAILURES: usize = 5;
const LISTED_WARNINGS: usize = 3;
const LISTED_PATHS: usize = 20;
const COMMAND_CHARS: usize = 60;
const PATH_CHARS: usize = 120;
const PATHS_BYTES: usize = 1200;
/// The largest summary line `finish` builds: five failures, five cut-off commands, two
/// path lists, three warnings and the counts. The tests hold it below this, which is far
/// below the 16 KiB the output ceiling keeps of the end of a result.
#[cfg(test)]
const SUMMARY_LINE_MAX_BYTES: usize = 4608;

/// The output ceiling keeps only the first [`crate::output::CAPTURE_BYTES`] of a result,
/// in its artifact and in the preview it takes from the end of that. The summary line is
/// the end of the result, so the result must never be longer than that: `finish` gives
/// the printed output what remains after everything else, each part bounded.
const CAPTURE_BYTES: usize = crate::output::CAPTURE_BYTES;
/// The returned value, at most this much, with its note.
const RETURN_BYTES: usize = 256 * 1024;
/// The error message and stack of a failed program, and the note about commands that were
/// not confirmed stopped, at most this much together.
const FAILURE_BYTES: usize = 16 * 1024;
/// Room for the line breaks, `=> `, the notes about cut texts and a margin.
const NOTES_BYTES: usize = 8 * 1024;
const DRAIN_NOTE_CHARS: usize = 1000;

/// `text` cut to at most `max` bytes on a character boundary, and how many bytes it lost.
fn cut_to(text: &str, max: usize) -> (&str, usize) {
    if text.len() <= max {
        return (text, 0);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], text.len() - end)
}

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
    let mut content = String::new();
    let summary = summary_line(&state);
    // Neither of these depends on the failure list, which leaves reasons out.
    for notice in [cut_off_notice(&state), failed_calls_notice(&state)]
        .into_iter()
        .flatten()
    {
        content.push_str(&notice);
        content.push('\n');
    }
    // Everything but the printed output is bounded, and the printed output gets what is
    // left of the capture limit, so the whole result, which ends with the summary line,
    // is never longer than the output ceiling keeps.
    let output_room = CAPTURE_BYTES
        .saturating_sub(content.len() + summary.len() + RETURN_BYTES + FAILURE_BYTES + NOTES_BYTES);
    let output = std::mem::take(&mut state.output);
    let (kept, cut_here) = cut_to(&output, output_room);
    let output_bytes = output.len();
    let dropped = state.output_dropped.saturating_add(cut_here as u64);
    content.push_str(kept);
    if !kept.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if dropped > 0 {
        content.push_str(&format!(
            "[output truncated by run_js: {dropped} bytes dropped]\n"
        ));
        state.limit_hit.get_or_insert("output");
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
                let failed = if error.stack.trim().is_empty() {
                    message
                } else {
                    format!("{message}\n{}", error.stack.trim_end())
                };
                failure = Some(bounded_failure(failed));
            } else if timed_out {
                failure = Some(timeout_message(timeout_ms, limits));
            } else if !value.is_null() {
                let text = match &value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                return_bytes = text.len();
                content.push_str("=> ");
                let start = content.len();
                let (kept, cut) = cut_to(&text, RETURN_BYTES);
                content.push_str(kept);
                if cut > 0 {
                    content.push_str(&format!(
                        "\n[returned value truncated by run_js: {cut} bytes dropped]"
                    ));
                }
                returned = Some((start, content.len()));
                content.push('\n');
            }
        }
        Ending::Failed(reason) => {
            let stderr = if stderr.is_empty() {
                String::new()
            } else {
                format!(" (worker stderr: {stderr})")
            };
            failure = Some(bounded_failure(format!("{reason}{stderr}")));
        }
    }
    if let Some(error) = &state.drain_error {
        let note = format!(
            "run_js could not confirm that the commands it started have stopped ({}); \
treat their effects as unknown",
            one_line(error, DRAIN_NOTE_CHARS)
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
    content.push_str(&summary);
    debug_assert!(content.len() <= CAPTURE_BYTES, "{} bytes", content.len());
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
    let result = result.with_detail(detail);
    if state.cut_off.is_empty() {
        return result;
    }
    // Exactly the commands the summary line names: the first few, the rest only counted.
    result.with_cut_off_told(CutOffTold {
        seqs: state
            .cut_off
            .iter()
            .take(LISTED_CUT_OFF)
            .map(|cut| cut.seq)
            .collect(),
        summary_line: summary,
    })
}

/// A failure text cut to what the result reserves for it, so it cannot crowd out the
/// summary line.
fn bounded_failure(text: String) -> String {
    let (kept, cut) = cut_to(&text, FAILURE_BYTES - 2 * DRAIN_NOTE_CHARS);
    if cut == 0 {
        return text;
    }
    format!("{kept}\n[failure text truncated by run_js: {cut} bytes dropped]")
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

/// Commands named in a cut-off notice before the rest are only counted.
const LISTED_CUT_OFF: usize = 5;

/// What the model is told of the shell commands that were running when the program
/// ended and were stopped: each by its call number and the start of its command, and that
/// it may have partly run. This is the acknowledgement that lets the host stop treating
/// their effects as unseen, so it is exactly what [`delivered_cut_off_shells`] checks for,
/// and it is bounded but never left out.
fn cut_off_list(state: &Run) -> Option<String> {
    if state.cut_off.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = state
        .cut_off
        .iter()
        .take(LISTED_CUT_OFF)
        .map(|cut| {
            format!(
                "shell #{} `{}`",
                cut.seq,
                one_line(&cut.command, COMMAND_CHARS)
            )
        })
        .collect();
    if state.cut_off.len() > LISTED_CUT_OFF {
        parts.push(format!("+{} more", state.cut_off.len() - LISTED_CUT_OFF));
    }
    Some(parts.join("; "))
}

fn cut_off_notice(state: &Run) -> Option<String> {
    let list = cut_off_list(state)?;
    let count = state.cut_off.len();
    Some(format!(
        "{count} shell command{} cut off when the program ended and may have partly run: {list}",
        if count == 1 { " was" } else { "s were" }
    ))
}

/// The call numbers of the cut-off shell commands that `result` really tells the model
/// about. `run_js` records them, with the summary line it put them on, in the result's
/// runtime-only facts, which only the tool can set; they count if the result that is
/// being added to the history still ends with that line. A result without them, such as
/// the executor's "stopped" or "cancelled" or a failed task's error, acknowledges nothing.
/// Nothing in the text, which a program or a file name can fill, is trusted.
pub fn acknowledged_cut_off(result: &ToolResult) -> Vec<u32> {
    let Some(told) = result.cut_off_told() else {
        return Vec::new();
    };
    // The output ceiling keeps the end of a long result and adds a note about its artifact
    // after it; the summary line is what comes just before that note.
    let text = result.content.trim_end();
    let kept = text.ends_with(&told.summary_line)
        || (result
            .detail
            .as_ref()
            .is_some_and(|detail| detail.get("output_artifact").is_some())
            && CEILING_NOTES.iter().any(|note| {
                text.rfind(note)
                    .is_some_and(|at| text[..at].trim_end().ends_with(&told.summary_line))
            }));
    if kept { told.seqs.clone() } else { Vec::new() }
}

/// What the output ceiling appends after the preview it keeps (see `output.rs`).
const CEILING_NOTES: [&str; 2] = ["\n[Output artifact: ", "\n[Output truncated;"];

/// One line, however much went wrong: reasons are cut and listed up to a limit.
fn summary_line(state: &Run) -> String {
    let failed = state.calls.iter().filter(|call| call.is_error).count();
    let mut line = format!("[run_js: {} calls, {failed} failed", state.calls.len());
    if !state.failures.is_empty() {
        line.push_str(&format!(" ({})", failure_list(&state.failures)));
    }
    if let Some(list) = cut_off_list(state) {
        line.push_str(&format!("; cut off, may have partly run: {list}"));
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

/// Paths named in one list of the summary line: at most [`LISTED_PATHS`] of at most
/// [`PATH_CHARS`] characters and [`PATHS_BYTES`] bytes in all, the rest counted. Every list
/// of the line has such a bound, so the line stays far below the preview the output
/// ceiling keeps of the end of a long result; the full lists are in the detail.
fn join_paths(paths: &std::collections::BTreeSet<String>) -> String {
    let mut shown = String::new();
    let mut listed = 0;
    for path in paths.iter().take(LISTED_PATHS) {
        let path = one_line(path, PATH_CHARS);
        let separator = if shown.is_empty() { 0 } else { 2 };
        if listed > 0 && shown.len() + separator + path.len() > PATHS_BYTES {
            break;
        }
        if separator > 0 {
            shown.push_str(", ");
        }
        shown.push_str(&path);
        listed += 1;
    }
    if paths.len() > listed {
        shown.push_str(&format!("; +{} more", paths.len() - listed));
    }
    shown
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
    let cut_off: Vec<Value> = state
        .cut_off
        .iter()
        .map(|cut| json!({"seq": cut.seq, "command": cut.command}))
        .collect();
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
            "cut_off": cut_off,
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

    fn cut_off(seq: u32, command: &str) -> super::super::serve::CutOff {
        super::super::serve::CutOff {
            seq,
            command: command.into(),
        }
    }

    /// A run whose calls 1..=`failed` failed and whose call `failed + 1`, a shell command,
    /// was cut off.
    fn run_with_failures_and_cut_off(failed: u32, cut: Vec<super::super::serve::CutOff>) -> Run {
        let mut state = Run::default();
        for seq in 1..=failed {
            let (call, failure) = failed_call(seq, "read", &format!("no such file {seq}"));
            state.calls.push(call);
            state.failures.push(failure);
        }
        for entry in &cut {
            let (call, failure) = failed_call(entry.seq, "shell", "cut off before it finished");
            state.calls.push(call);
            state.failures.push(failure);
        }
        state.cut_off = cut;
        state
    }

    #[test]
    fn a_cut_off_shell_is_named_even_when_five_failures_fill_the_failure_list() {
        let state =
            run_with_failures_and_cut_off(5, vec![cut_off(6, "cargo check --offline -p tools")]);
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let text = &result.content;
        // The failure list shows five reasons and counts the rest: the shell is in "+1 more".
        assert!(text.contains("+1 more"), "{text}");
        assert!(!text.contains("shell #6: cut off"), "{text}");
        // It is named apart from that list, at the top and in the summary line.
        assert!(
            text.starts_with("1 shell command was cut off when the program ended and may have partly run: shell #6 `cargo check --offline -p tools`\n"),
            "{text}"
        );
        let last = text.lines().last().unwrap();
        assert!(
            last.contains(
                "; cut off, may have partly run: shell #6 `cargo check --offline -p tools`"
            ),
            "{last}"
        );
        assert_eq!(acknowledged_cut_off(&result), [6]);
        assert_eq!(
            result.detail.as_ref().unwrap()["run_js"]["cut_off"],
            json!([{"seq": 6, "command": "cargo check --offline -p tools"}])
        );
    }

    #[test]
    fn many_cut_off_commands_are_bounded_and_only_the_named_ones_count_as_told() {
        let cut: Vec<_> = (1..=7)
            .map(|seq| cut_off(seq, &format!("sleep {seq}")))
            .collect();
        let result = finish(
            run_with_failures_and_cut_off(0, cut),
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let text = &result.content;
        assert!(text.starts_with("7 shell commands were cut off"), "{text}");
        assert!(text.contains("shell #5 `sleep 5`; +2 more"), "{text}");
        assert!(
            !text.contains("shell #6 `"),
            "the rest are counted, not named: {text}"
        );
        // Only the five the model was told about are acknowledged.
        assert_eq!(acknowledged_cut_off(&result), [1, 2, 3, 4, 5]);
    }

    fn many_long_paths(count: usize, tag: &str) -> std::collections::BTreeSet<String> {
        (0..count)
            .map(|index| format!("/work/{tag}/{index:04}/{}", "d".repeat(180)))
            .collect()
    }

    #[test]
    fn a_long_changed_file_list_cannot_push_the_cut_off_notice_out_of_a_cut_down_result() {
        let mut state = run_with_failures_and_cut_off(5, vec![cut_off(6, "cargo check")]);
        state.written = many_long_paths(500, "w");
        state.uncertain = many_long_paths(500, "u");
        state.output = "x\n".repeat(60_000);
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let summary = result.content.trim_end().lines().last().unwrap().to_owned();
        assert!(
            summary.len() <= SUMMARY_LINE_MAX_BYTES,
            "{} bytes",
            summary.len()
        );
        assert!(
            summary.contains("; +491 more"),
            "the lists are counted, not dumped: {summary}"
        );

        let limited = crate::output::limit_result(result);

        assert!(
            limited.content.contains("[Output artifact: "),
            "it was cut to a preview"
        );
        let before_note = limited
            .content
            .rsplit_once("\n[Output artifact: ")
            .unwrap()
            .0;
        assert!(
            before_note.ends_with(&summary),
            "the whole summary line is what the preview ends with"
        );
        assert_eq!(acknowledged_cut_off(&limited), [6]);
    }

    #[test]
    fn the_summary_line_stays_bounded_whatever_the_lists_hold() {
        let mut state = run_with_failures_and_cut_off(
            40,
            (41..=50)
                .map(|seq| cut_off(seq, &"c".repeat(500)))
                .collect(),
        );
        for seq in 1..=40 {
            state.failures[usize::try_from(seq - 1).unwrap()].reason = "r".repeat(1000);
        }
        state.written = many_long_paths(2000, "w");
        state.uncertain = many_long_paths(2000, "u");
        state
            .warnings
            .extend((1..=10).map(|seq| (seq, "edit".to_owned(), "w".repeat(1000))));
        let line = summary_line(&state);
        assert!(line.len() <= SUMMARY_LINE_MAX_BYTES, "{} bytes", line.len());
        assert!(line.ends_with(']') && !line.contains('\n'));

        // At that size, and after a huge output, the output ceiling still keeps the whole
        // line, which is what lets the host rely on it.
        state.output = "x\n".repeat(400_000);
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let limited = crate::output::limit_result(result);
        assert!(limited.content.contains("[Output artifact: "));
        assert!(
            limited.content.contains(&line),
            "the whole line is in the preview"
        );
        assert_eq!(acknowledged_cut_off(&limited), [41, 42, 43, 44, 45]);
    }

    /// The run of the capture-limit tests: failed calls, one cut-off command (seq 6) and
    /// long written paths, so the summary line is as long as it gets.
    fn heavy_state() -> Run {
        let mut state = run_with_failures_and_cut_off(5, vec![cut_off(6, "cargo check")]);
        state.written = many_long_paths(500, "w");
        state.uncertain = many_long_paths(500, "u");
        state
    }

    /// What the output ceiling must keep of `result`: the summary line is the end of both
    /// the preview and the artifact (when one was saved), and the host acknowledges.
    fn assert_summary_survives(result: ToolResult, what: &str) {
        let summary = result.content.lines().last().unwrap().to_owned();
        assert!(summary.starts_with("[run_js: "), "{what}");
        assert!(
            result.content.len() <= CAPTURE_BYTES,
            "{what}: whole result is in capture"
        );
        let limited = crate::output::limit_result(result);
        let Some(artifact) = limited
            .detail
            .as_ref()
            .and_then(|detail| detail.get("output_artifact").cloned())
        else {
            // Small enough to be returned whole.
            assert!(limited.content.ends_with(&summary), "{what}");
            assert_eq!(acknowledged_cut_off(&limited), [6], "{what}");
            return;
        };
        let kept = limited
            .content
            .rsplit_once("\n[Output ")
            .unwrap_or_else(|| panic!("{what}: no note after the preview"))
            .0;
        assert!(
            kept.ends_with(&summary),
            "{what}: the preview ends with the summary line"
        );
        assert_eq!(acknowledged_cut_off(&limited), [6], "{what}");
        if let Some(path) = artifact["path"].as_str() {
            assert_eq!(
                artifact["complete"], true,
                "{what}: the artifact has the whole result"
            );
            let saved = std::fs::read_to_string(path).unwrap();
            assert!(
                saved.ends_with(&summary),
                "{what}: the artifact ends with the summary line"
            );
        }
    }

    #[test]
    fn the_summary_line_survives_output_before_at_and_past_the_capture_limit() {
        let base = finish(
            heavy_state(),
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let room = CAPTURE_BYTES - base.content.len() - RETURN_BYTES - FAILURE_BYTES - NOTES_BYTES;
        for (size, dropped) in [
            (room - 1, 0),
            (room, 0),
            (room + 1, 1),
            (CAPTURE_BYTES - 1, CAPTURE_BYTES - 1 - room),
            (CAPTURE_BYTES, CAPTURE_BYTES - room),
            (CAPTURE_BYTES + 1, CAPTURE_BYTES + 1 - room),
        ] {
            let mut state = heavy_state();
            state.output = "x".repeat(size);
            let result = finish(
                state,
                done(Value::Null),
                "",
                1000,
                &config::RunJsConfig::default(),
            );
            let note = format!("[output truncated by run_js: {dropped} bytes dropped]");
            assert_eq!(
                result.content.contains("[output truncated by run_js:"),
                dropped > 0,
                "{size}"
            );
            if dropped > 0 {
                assert!(result.content.contains(&note), "{size}: {dropped}");
                assert_eq!(
                    result.detail.as_ref().unwrap()["run_js"]["limit_hit"],
                    "output"
                );
            }
            assert_summary_survives(result, &format!("output of {size} bytes"));
        }
    }

    #[test]
    fn dropped_bytes_the_host_never_kept_are_counted_in_the_note() {
        let mut state = heavy_state();
        state.output = "x".repeat(1000);
        state.output_dropped = 7_000_000;
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert!(
            result
                .content
                .contains("[output truncated by run_js: 7000000 bytes dropped]")
        );
        assert_summary_survives(result, "host-dropped output");
    }

    #[test]
    fn a_huge_return_value_and_error_text_share_the_budget_without_pushing_out_the_summary() {
        let mut state = heavy_state();
        state.output = "x".repeat(CAPTURE_BYTES);
        let value = Value::String("r".repeat(20 * 1024 * 1024));
        let result = finish(
            state,
            done(value),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert!(
            result
                .content
                .contains("[returned value truncated by run_js: ")
        );
        let layout = &result.detail.as_ref().unwrap()["run_js"]["layout"];
        let [start, end] = [
            layout["returned"][0].as_u64().unwrap(),
            layout["returned"][1].as_u64().unwrap(),
        ];
        assert!(
            end - start <= (RETURN_BYTES + 100) as u64,
            "the returned range is bounded"
        );
        assert_summary_survives(result, "huge returned value");

        let mut state = heavy_state();
        state.output = "x".repeat(CAPTURE_BYTES);
        state.drain_error = Some("d".repeat(100_000));
        let error = JsError {
            message: "m".repeat(5_000_000),
            stack: "s".repeat(5_000_000),
        };
        let ending = Ending::Done {
            value: Value::Null,
            error: Some(error),
            timed_out: false,
        };
        let result = finish(state, ending, "", 1000, &config::RunJsConfig::default());
        assert!(
            result
                .content
                .contains("[failure text truncated by run_js: ")
        );
        assert!(
            result
                .content
                .contains("could not confirm that the commands it started have stopped")
        );
        assert!(result.is_error);
        assert_summary_survives(result, "huge error text");
    }

    #[test]
    fn the_summary_line_survives_when_the_artifact_cannot_be_saved() {
        // The output directory is under a regular file, so nothing can be saved there.
        const CHILD: &str = "EVORCH_SUMMARY_TEST";
        let test = "tools::run_js::summary::tests::the_summary_line_survives_when_the_artifact_cannot_be_saved";
        if std::env::var(CHILD).is_err() {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("file");
            std::fs::write(&file, "x").unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(CHILD, "1")
                .env("EVORCH_OUTPUT_DIR", file.join("below"))
                .status()
                .unwrap();
            assert!(status.success(), "isolated process failed");
            return;
        }
        let base = finish(
            heavy_state(),
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        let room = CAPTURE_BYTES - base.content.len() - RETURN_BYTES - FAILURE_BYTES - NOTES_BYTES;
        for size in [room, room + 1, CAPTURE_BYTES + 1] {
            let mut state = heavy_state();
            state.output = "x".repeat(size);
            let result = finish(
                state,
                done(Value::Null),
                "",
                1000,
                &config::RunJsConfig::default(),
            );
            let summary = result.content.lines().last().unwrap().to_owned();
            let limited = crate::output::limit_result(result);
            assert!(limited.content.contains("Artifact unavailable"), "{size}");
            let kept = limited
                .content
                .rsplit_once("\n[Output truncated;")
                .unwrap()
                .0;
            assert!(
                kept.ends_with(&summary),
                "{size}: the preview still ends with the summary line"
            );
            assert_eq!(acknowledged_cut_off(&limited), [6], "{size}");
        }
    }

    #[test]
    fn a_fragment_in_a_file_name_or_the_output_acknowledges_nothing() {
        // No command was cut off; the text says it did.
        let mut state = Run {
            output: "shell #1 `cargo check` was cut off and may have partly run\n".into(),
            ..Run::default()
        };
        state.written.insert("/w/shell #1 `cargo check`.rs".into());
        let result = finish(
            state,
            done(Value::Null),
            "",
            1000,
            &config::RunJsConfig::default(),
        );
        assert!(result.content.contains("shell #1 `cargo check`"));
        assert!(result.cut_off_told().is_none());
        assert!(acknowledged_cut_off(&result).is_empty());

        // A result nobody made with `run_js`' facts: the executor's replacements, and a
        // detail and text that merely look right.
        for text in ["stopped", "cancelled", "task failed"] {
            assert!(acknowledged_cut_off(&ToolResult::error(text)).is_empty());
        }
        let forged = ToolResult::error("... shell #3 `make` ...")
            .with_detail(json!({"run_js": {"cut_off": [{"seq": 3, "command": "make"}]}}));
        assert!(acknowledged_cut_off(&forged).is_empty());

        // The facts are there but the result no longer ends with the line (replaced text).
        let replaced = ToolResult::error("something else").with_cut_off_told(CutOffTold {
            seqs: vec![3],
            summary_line:
                "[run_js: 1 calls, 1 failed; cut off, may have partly run: shell #3 `make`]".into(),
        });
        assert!(acknowledged_cut_off(&replaced).is_empty());
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
