//! The `run_js` card (ADR 0039): the code, the tool calls it made, and what it
//! returned to the model.
//!
//! Child calls are ordinary transcript entries whose `parent_call_id` names
//! the `run_js` call. They keep their own cards (an `edit` shows its diff, a
//! `shell` its command and live output) and appear inside the parent card in
//! the order they started.

use std::collections::{HashMap, HashSet};

use egui::{RichText, Ui};

use super::{
    INTERRUPTED_META, INTERRUPTED_NOTE, ToolCardContext, code, fitted_header_job, header,
    pretty_json, review_line, status_color, status_mark, sticky, tool_card_group,
};
use crate::model::transcript::{ToolStatus, TranscriptEntry};
use crate::theme::tokens::{FONT_SMALL, R_SM, SP_1, palette};

pub const RUN_JS: &str = "run_js";

/// Whether `entry` is drawn with the run_js layout. A call still being
/// composed has no program yet, so it keeps the generic "composing…" card.
pub fn uses_run_js_layout(entry: &TranscriptEntry) -> bool {
    matches!(
        entry,
        TranscriptEntry::Tool { tool_name, status, .. }
            if tool_name == RUN_JS && !matches!(status, ToolStatus::Drafting { .. })
    )
}

/// Which entries are `run_js` children shown inside their parent's card.
///
/// A child whose parent is not among the entries (a restore still in
/// progress, or a parent outside the visible window) is not nested and
/// renders as a top-level card.
#[derive(Debug, Default)]
pub struct RunJsNesting {
    children: HashMap<usize, Vec<usize>>,
    nested: HashSet<usize>,
}

impl RunJsNesting {
    pub fn new(entries: &[TranscriptEntry]) -> Self {
        let mut parents = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if let TranscriptEntry::Tool {
                tool_name,
                run_id,
                call_id,
                parent_call_id: None,
                ..
            } = entry
                && tool_name == RUN_JS
            {
                // A reused call ID belongs to its latest call, as elsewhere.
                parents.insert((run_id.as_deref(), call_id.as_str()), index);
            }
        }
        let mut nesting = Self::default();
        for (index, entry) in entries.iter().enumerate() {
            if let TranscriptEntry::Tool {
                run_id,
                parent_call_id: Some(parent),
                ..
            } = entry
                && let Some(&parent) = parents.get(&(run_id.as_deref(), parent.as_str()))
            {
                nesting.children.entry(parent).or_default().push(index);
                nesting.nested.insert(index);
            }
        }
        nesting
    }

    /// Whether the entry at `index` renders inside its parent's card.
    pub fn is_nested(&self, index: usize) -> bool {
        self.nested.contains(&index)
    }

    /// Indices of the children of the `run_js` entry at `index`, in the
    /// order they started.
    pub fn children(&self, index: usize) -> &[usize] {
        self.children.get(&index).map_or(&[], Vec::as_slice)
    }
}

/// A child call to draw inside a `run_js` card.
#[derive(Clone, Copy)]
pub struct RunJsChild<'a> {
    pub entry: &'a TranscriptEntry,
    /// Whether the child is the latest card about its shell job.
    pub latest_for_job: bool,
}

/// What the run reported about its calls, from the completed detail or,
/// while it runs, from the children seen so far.
#[derive(Debug, Default, PartialEq, Eq)]
struct Stats {
    calls: usize,
    failed: usize,
    timed_out: bool,
    /// Which limit stopped the run (`tool_calls`, `timeout`, `memory`, `output`);
    /// empty when only the fact is known.
    limit_hit: Option<String>,
}

impl Stats {
    fn new(detail: Option<&serde_json::Value>, children: &[RunJsChild<'_>]) -> Self {
        let run = detail.and_then(|detail| detail.get("run_js"));
        let flag = |key| {
            run.and_then(|run| run.get(key))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        let count = |key| {
            run.and_then(|run| run.get(key))
                .and_then(serde_json::Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
        };
        let (calls, failed) = match run
            .and_then(|run| run.get("calls"))
            .and_then(serde_json::Value::as_array)
        {
            // The list may be cut to fit the detail; the counts are not.
            Some(calls) => (
                calls.len() + count("calls_omitted").unwrap_or(0),
                count("calls_failed").unwrap_or_else(|| {
                    calls
                        .iter()
                        .filter(|call| {
                            call.get("is_error").and_then(serde_json::Value::as_bool) == Some(true)
                        })
                        .count()
                }),
            ),
            None => (
                children.len(),
                children
                    .iter()
                    .filter(|child| {
                        matches!(
                            child.entry,
                            TranscriptEntry::Tool { is_error: true, .. }
                                | TranscriptEntry::Tool {
                                    status: ToolStatus::Failed | ToolStatus::Denied { .. },
                                    ..
                                }
                        )
                    })
                    .count(),
            ),
        };
        Self {
            calls,
            failed,
            timed_out: flag("timed_out"),
            limit_hit: match run.and_then(|run| run.get("limit_hit")) {
                Some(serde_json::Value::String(kind)) => Some(kind.clone()),
                Some(serde_json::Value::Bool(true)) => Some(String::new()),
                _ => None,
            },
        }
    }

    fn meta(&self, running: bool) -> Option<String> {
        let mut parts = Vec::new();
        if self.calls > 0 || !running {
            parts.push(match self.calls {
                1 => "1 call".to_owned(),
                count => format!("{count} calls"),
            });
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.timed_out {
            parts.push("timed out".to_owned());
        }
        match self.limit_hit.as_deref() {
            Some("") => parts.push("limit hit".to_owned()),
            Some(kind) => parts.push(format!("limit hit: {kind}")),
            None => {}
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The model-facing result split into its parts: console output, then
/// `=> <return value JSON>`, then the `[run_js: …]` summary line.
#[derive(Debug, PartialEq, Eq)]
struct Sections<'a> {
    console: &'a str,
    returned: Option<&'a str>,
    summary: Option<&'a str>,
}

fn sections(output: &str) -> Sections<'_> {
    let trimmed = output.trim_end();
    let (rest, summary) = match trimmed.rfind('\n').map_or((0, 0), |at| (at, at + 1)) {
        (end, start) if trimmed[start..].starts_with("[run_js:") => {
            (&trimmed[..end], Some(&trimmed[start..]))
        }
        _ => (trimmed, None),
    };
    let returned_at = if rest.starts_with("=> ") {
        Some(0)
    } else {
        rest.rfind("\n=> ").map(|at| at + 1)
    };
    match returned_at {
        Some(at) => Sections {
            console: rest[..at].trim_end_matches('\n'),
            returned: Some(rest[at + 3..].trim()),
            summary,
        },
        None => Sections {
            console: rest,
            returned: None,
            summary,
        },
    }
}

/// The code, the child calls and the result in one card. Child calls show
/// whether or not the card is expanded, so a running call's progress is
/// visible as it happens; the code and result need expanding.
pub fn run_js_card(
    ui: &mut Ui,
    entry: &TranscriptEntry,
    children: &[RunJsChild<'_>],
    pane_id: egui::Id,
    context: ToolCardContext<'_>,
) {
    let TranscriptEntry::Tool {
        tool_name,
        call_id,
        input,
        output,
        detail,
        is_error,
        status,
        run_id,
        reviews,
        ..
    } = entry
    else {
        return;
    };
    let id = pane_id.with(("tool-expanded", run_id, call_id));
    let running = matches!(status, ToolStatus::Running);
    let mut expanded = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    let color = status_color(status, *is_error);
    let mut header = header::tool_header(
        tool_name,
        input.as_ref(),
        output.as_deref(),
        *is_error,
        context.repo_root,
        context.agents,
    );
    header.meta = Stats::new(detail.as_ref(), children).meta(running);
    if matches!(status, ToolStatus::Interrupted) {
        header.meta = Some(
            [header.meta.take(), Some(INTERRUPTED_META.to_owned())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    let header_text = header.text();
    let (glyph, mark) = status_mark(status, *is_error, header.icon);
    let label = format!("{mark}{header_text}");
    let source = input
        .as_ref()
        .and_then(|input| input.get("code"))
        .and_then(serde_json::Value::as_str);
    crate::theme::widgets::soft_frame(palette().SURFACE).show(ui, |ui| {
        let title = sticky::Header::reserve(ui, id);
        for review in reviews {
            review_line(ui, review);
        }
        if expanded {
            match (source, input) {
                (Some(source), _) => {
                    ui.label("Code");
                    code(ui, source, palette().TEXT);
                }
                (None, Some(input)) => {
                    ui.label("Input");
                    code(ui, &pretty_json(input), palette().TEXT);
                }
                (None, None) => {}
            }
        }
        if !children.is_empty() {
            if expanded {
                ui.label("Calls");
            }
            egui::Frame::new()
                .fill(palette().SURFACE_RAISED)
                .corner_radius(R_SM)
                .inner_margin(SP_1)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for child in children {
                        let TranscriptEntry::Tool {
                            call_id: child_call,
                            ..
                        } = child.entry
                        else {
                            continue;
                        };
                        tool_card_group(
                            ui,
                            std::slice::from_ref(child.entry),
                            id.with(("run-js-child", child_call)),
                            ToolCardContext {
                                latest_for_job: child.latest_for_job,
                                ..context
                            },
                        );
                    }
                });
        }
        if expanded && !running {
            result(ui, output.as_deref(), *is_error);
            if let ToolStatus::Denied { reason } = status {
                code(ui, reason, palette().ERROR_FG);
            }
            if matches!(status, ToolStatus::Interrupted) {
                ui.colored_label(palette().WARNING_FG, INTERRUPTED_NOTE);
            }
        }
        let response = title.show(ui, expanded, |ui| {
            if running {
                crate::theme::widgets::spinner_with(ui, FONT_SMALL, Some(palette().RUNNING));
            }
            let width = ui.available_width() - ui.spacing().button_padding.x * 2.0;
            let layout = fitted_header_job(ui, glyph, color, &header, width);
            // Unlike other calls, the code is worth reading while it runs.
            let button = ui.add(egui::Button::new(layout).frame(false).truncate());
            button
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
            button.on_hover_text(&header_text)
        });
        if response.inner.clicked() {
            if expanded {
                title.reveal_after_collapse(ui);
            }
            expanded = !expanded;
            ui.data_mut(|data| data.insert_temp(id, expanded));
            ui.ctx().request_repaint();
        }
    });
}

fn result(ui: &mut Ui, output: Option<&str>, is_error: bool) {
    let Some(output) = output else {
        return;
    };
    if is_error {
        ui.label("Error");
        code(ui, output, palette().ERROR_FG);
        return;
    }
    let sections = sections(output);
    if !sections.console.is_empty() {
        ui.label("Output");
        code(ui, sections.console, palette().TEXT);
    }
    if let Some(returned) = sections.returned {
        ui.label("Return value");
        let pretty = serde_json::from_str::<serde_json::Value>(returned)
            .map(|value| pretty_json(&value))
            .unwrap_or_else(|_| returned.to_owned());
        code(ui, &pretty, palette().TEXT);
    }
    if let Some(summary) = sections.summary {
        ui.add(
            egui::Label::new(
                RichText::new(summary)
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .wrap(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(call_id: &str, tool_name: &str, parent: Option<&str>) -> TranscriptEntry {
        TranscriptEntry::Tool {
            run_id: Some("run-1".into()),
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Running,
            reviews: Vec::new(),
            parent_call_id: parent.map(str::to_owned),
        }
    }

    #[test]
    fn children_nest_under_their_run_js_parent_and_orphans_stay_top_level() {
        let entries = [
            tool("js", RUN_JS, None),
            tool("other", "read", None),
            tool("js/1", "read", Some("js")),
            tool("js/2", "shell", Some("js")),
            tool("gone/1", "grep", Some("gone")),
        ];
        let nesting = RunJsNesting::new(&entries);
        assert_eq!(nesting.children(0), &[2, 3]);
        assert!(nesting.is_nested(2) && nesting.is_nested(3));
        assert!(!nesting.is_nested(1));
        // The parent is not loaded (yet), so the child stays visible.
        assert!(!nesting.is_nested(4));
    }

    #[test]
    fn children_of_another_run_with_the_same_call_id_do_not_nest() {
        let mut stranger = tool("js/1", "read", Some("js"));
        if let TranscriptEntry::Tool { run_id, .. } = &mut stranger {
            *run_id = Some("run-2".into());
        }
        let nesting = RunJsNesting::new(&[tool("js", RUN_JS, None), stranger]);
        assert!(!nesting.is_nested(1));
    }

    #[test]
    fn output_splits_into_console_return_value_and_summary() {
        let output =
            "line one\n=> line two\n=> {\"ok\":true}\n[run_js: 2 calls, 0 failed; wrote: a.rs]\n";
        assert_eq!(
            sections(output),
            Sections {
                console: "line one\n=> line two",
                returned: Some("{\"ok\":true}"),
                summary: Some("[run_js: 2 calls, 0 failed; wrote: a.rs]"),
            }
        );
        assert_eq!(
            sections("=> 3\n[run_js: 0 calls, 0 failed]"),
            Sections {
                console: "",
                returned: Some("3"),
                summary: Some("[run_js: 0 calls, 0 failed]"),
            }
        );
        // Text that does not follow the format stays whole.
        assert_eq!(
            sections("plain"),
            Sections {
                console: "plain",
                returned: None,
                summary: None,
            }
        );
    }

    #[test]
    fn a_leading_failure_notice_is_console_text_and_leaves_the_return_value_and_summary_alone() {
        let output = "1 of 2 tool calls failed: edit #1: old_string not found\nprogress\n=> done\n[run_js: 2 calls, 1 failed (edit #1: old_string not found)]";
        assert_eq!(
            sections(output),
            Sections {
                console: "1 of 2 tool calls failed: edit #1: old_string not found\nprogress",
                returned: Some("done"),
                summary: Some("[run_js: 2 calls, 1 failed (edit #1: old_string not found)]"),
            }
        );
        let only_notice =
            "1 of 1 tool calls failed: read #1: x\n=> 3\n[run_js: 1 calls, 1 failed (read #1: x)]";
        let parsed = sections(only_notice);
        assert_eq!(parsed.returned, Some("3"));
        assert_eq!(parsed.console, "1 of 1 tool calls failed: read #1: x");
    }

    #[test]
    fn limit_hit_is_read_as_the_name_of_the_limit() {
        let stats = |limit_hit: serde_json::Value| {
            Stats::new(
                Some(&json!({"run_js": {"calls": [], "limit_hit": limit_hit}})),
                &[],
            )
            .meta(false)
        };
        assert_eq!(
            stats(json!("tool_calls")).as_deref(),
            Some("0 calls · limit hit: tool_calls")
        );
        // The earlier boolean form still reads, and null or false mean no limit.
        assert_eq!(stats(json!(true)).as_deref(), Some("0 calls · limit hit"));
        assert_eq!(stats(json!(null)).as_deref(), Some("0 calls"));
        assert_eq!(stats(json!(false)).as_deref(), Some("0 calls"));
    }

    #[test]
    fn counts_include_calls_cut_from_the_listed_detail() {
        let detail = json!({"run_js": {
            "calls": [{"seq": 1, "name": "read", "is_error": false, "paths": []}],
            "calls_omitted": 199,
            "calls_failed": 12,
        }});
        assert_eq!(
            Stats::new(Some(&detail), &[]).meta(false).as_deref(),
            Some("200 calls · 12 failed")
        );
    }

    #[test]
    fn a_multi_line_string_return_value_runs_to_the_summary_line() {
        assert_eq!(
            sections("progress\n=> first line\nsecond line\n\n[run_js: 1 calls, 0 failed]"),
            Sections {
                console: "progress",
                returned: Some("first line\nsecond line"),
                summary: Some("[run_js: 1 calls, 0 failed]"),
            }
        );
    }

    #[test]
    fn stats_prefer_the_completed_detail_over_children_seen() {
        let mut failed = tool("js/1", "read", Some("js"));
        if let TranscriptEntry::Tool { is_error, .. } = &mut failed {
            *is_error = true;
        }
        let children = [RunJsChild {
            entry: &failed,
            latest_for_job: false,
        }];
        assert_eq!(
            Stats::new(None, &children).meta(true).as_deref(),
            Some("1 call · 1 failed")
        );
        let detail = json!({"run_js": {
            "calls": [
                {"seq": 1, "name": "read", "is_error": false, "paths": []},
                {"seq": 2, "name": "edit", "is_error": false, "paths": ["a.rs"]},
            ],
            "timed_out": true,
            "limit_hit": false,
        }});
        assert_eq!(
            Stats::new(Some(&detail), &children).meta(false).as_deref(),
            Some("2 calls · timed out")
        );
        assert_eq!(Stats::new(None, &[]).meta(true), None);
    }
}
