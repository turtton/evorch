use std::borrow::Cow;
use std::path::Path;

use egui::{
    Color32, FontId, RichText, Ui,
    text::{LayoutJob, TextFormat},
};

use crate::model::transcript::shell_jobs::{ShellJob, ShellJobKey, ShellJobs, result_body};
use crate::model::transcript::{ToolReview, ToolStatus, TranscriptEntry};
use crate::theme::icons;
use crate::theme::text::WEIGHT_MEDIUM;
use crate::theme::tokens::{FONT_BODY, FONT_SMALL, R_SM, ROW_DENSE, SP_2, palette};
use crate::theme::widgets::{ghost_icon_button, icon_button, soft_frame};

mod header;
mod orchestration;
mod run_js;
mod sticky;

pub use orchestration::agent_name;
pub use run_js::{RUN_JS, RunJsChild, RunJsNesting, run_js_card, uses_run_js_layout};

const LIVE_TAIL_LINES: usize = 4;
const DRAFTING_META: &str = "composing…";
/// Header note of a call whose run ended before it reported a result.
pub const INTERRUPTED_META: &str = "ended without a result";
const INTERRUPTED_NOTE: &str =
    "The run ended before this call reported a result, so its outcome is unknown.";
/// Room kept in a browser tool card's header for its View button.
const VIEW_BUTTON_WIDTH: f32 = 64.0;

use header::ToolHeader;

/// What a tool card needs beyond its entries.
#[derive(Clone, Copy)]
pub struct ToolCardContext<'a> {
    pub repo_root: Option<&'a Path>,
    pub jobs: &'a ShellJobs,
    /// Whether the card is the latest one about its shell job, which alone
    /// previews the job's live output.
    pub latest_for_job: bool,
    /// Resolves run IDs in coordination tool inputs to agent names.
    pub agents: &'a [crate::model::tasks::TaskRow],
}

pub fn tool_card(ui: &mut Ui, entry: &TranscriptEntry, pane_id: egui::Id) {
    let jobs = ShellJobs::default();
    let context = ToolCardContext {
        repo_root: None,
        jobs: &jobs,
        latest_for_job: false,
        agents: &[],
    };
    tool_card_group(ui, std::slice::from_ref(entry), pane_id, context);
}

/// The shell job an entry starts or controls.
pub fn shell_job_id(entry: &TranscriptEntry) -> Option<&str> {
    let TranscriptEntry::Tool {
        tool_name,
        input,
        detail,
        ..
    } = entry
    else {
        return None;
    };
    if !matches!(tool_name.as_str(), "bash" | "shell") {
        return None;
    }
    detail
        .as_ref()
        .and_then(|detail| detail.pointer("/shell_job/job_id"))
        .or_else(|| input.as_ref()?.get("job_id"))
        .and_then(serde_json::Value::as_str)
}

/// Scope the public handle to its owner and optional historical instance.
pub fn shell_job_key(entry: &TranscriptEntry) -> Option<ShellJobKey> {
    let handle = shell_job_id(entry)?;
    let TranscriptEntry::Tool { run_id, detail, .. } = entry else {
        return None;
    };
    let uid = detail
        .as_ref()
        .and_then(|detail| detail.pointer("/shell_job/job_uid"))
        .and_then(serde_json::Value::as_str);
    Some(ShellJobKey::new(run_id.as_deref(), handle, uid))
}

/// Only an unfinished control call may resolve a UUID from the live store.
/// Completed legacy results keep their exact historical identity.
///
/// An unfinished start call has no handle yet. Live output names the call
/// that started its job, which is how a `run_js` child shell streams before
/// its result arrives.
pub fn resolved_shell_job_key(entry: &TranscriptEntry, jobs: &ShellJobs) -> Option<ShellJobKey> {
    if let TranscriptEntry::Tool {
        run_id,
        tool_name,
        call_id,
        detail: None,
        status: ToolStatus::Running,
        ..
    } = entry
    {
        if shell_control(entry).is_some() {
            return jobs
                .get_scoped(run_id.as_deref(), shell_job_id(entry)?)
                .map(|job| job.key.clone());
        }
        if matches!(tool_name.as_str(), "bash" | "shell") && shell_job_id(entry).is_none() {
            return jobs
                .get_by_call(run_id.as_deref(), call_id)
                .map(|job| job.key.clone());
        }
    }
    shell_job_key(entry)
}

fn shell_control(entry: &TranscriptEntry) -> Option<&str> {
    let TranscriptEntry::Tool { input, .. } = entry else {
        return None;
    };
    input
        .as_ref()?
        .get("action")?
        .as_str()
        .filter(|action| matches!(*action, "poll" | "stdin" | "stop"))
}

/// Read-only status checks that agents repeat while waiting for something.
const POLLING_TOOLS: &[&str] = &[
    "inbox",
    "user_answers",
    "wait",
    "wait_reply",
    "run_output",
    "subagent_questions",
    "list_agents",
    "inspect_agent",
    "get_goal",
    "working_note_read",
];

/// What makes two finished polls the same observation: caller, tool, target and
/// the state they reported. Failures and running calls never match.
fn poll_signature(
    entry: &TranscriptEntry,
) -> Option<(Option<&str>, &str, Option<&serde_json::Value>, &str)> {
    let TranscriptEntry::Tool {
        run_id,
        tool_name,
        input,
        output: Some(output),
        is_error: false,
        status: ToolStatus::Succeeded,
        parent_call_id: None,
        ..
    } = entry
    else {
        return None;
    };
    POLLING_TOOLS.contains(&tool_name.as_str()).then_some((
        run_id.as_deref(),
        tool_name,
        input.as_ref(),
        output,
    ))
}

/// How many entries from the start of `entries` share one card: consecutive
/// polls of the same shell job fold together, as do identical read-only status
/// checks that observed the same state; everything else stands alone.
pub fn card_len(entries: &[TranscriptEntry], jobs: &ShellJobs) -> usize {
    let Some(first) = entries.first() else {
        return 0;
    };
    // A run_js child belongs to its parent's card, so it never folds.
    let top_level_poll = |entry: &TranscriptEntry| {
        shell_control(entry) == Some("poll")
            && matches!(
                entry,
                TranscriptEntry::Tool {
                    parent_call_id: None,
                    ..
                }
            )
    };
    let Some(job) = resolved_shell_job_key(first, jobs).filter(|_| top_level_poll(first)) else {
        let Some(signature) = poll_signature(first) else {
            return 1;
        };
        return 1 + entries[1..]
            .iter()
            .take_while(|entry| poll_signature(entry) == Some(signature))
            .count();
    };
    1 + entries[1..]
        .iter()
        .take_while(|entry| {
            top_level_poll(entry) && resolved_shell_job_key(entry, jobs).as_ref() == Some(&job)
        })
        .count()
}

/// Renders one card for `group`, a single call or a run of folded polls
/// (see [`card_len`]). A folded card reports the latest poll.
pub fn tool_card_group(
    ui: &mut Ui,
    group: &[TranscriptEntry],
    pane_id: egui::Id,
    context: ToolCardContext<'_>,
) {
    let Some(
        last @ TranscriptEntry::Tool {
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
        },
    ) = group.last()
    else {
        return;
    };
    if uses_run_js_layout(last) {
        // Callers without the transcript's nesting still get the run_js layout.
        run_js_card(ui, last, &[], pane_id, context);
        return;
    }
    let folded = group.len();
    let id = pane_id.with(("tool-expanded", run_id, call_id));
    let running = matches!(status, ToolStatus::Running | ToolStatus::Drafting { .. });
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
    let last = &group[folded - 1];
    let job = resolved_shell_job_key(last, context.jobs).and_then(|key| context.jobs.get_key(&key));
    if let Some(job) = job {
        header::with_shell_job(&mut header, job, shell_control(last).is_some());
    }
    if matches!(status, ToolStatus::Drafting { .. }) {
        header.meta = Some(
            [header.meta.take(), Some(DRAFTING_META.to_owned())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    if matches!(status, ToolStatus::Interrupted) {
        header.meta = Some(
            [header.meta.take(), Some(INTERRUPTED_META.to_owned())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    if folded > 1 {
        let outcome = header.meta.take();
        header.meta = Some(
            [Some(format!("×{folded}")), outcome]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · "),
        );
    }
    let header_text = header.text();
    let (glyph, mark) = status_mark(status, *is_error, header.icon);
    let label = format!("{mark}{header_text}");
    let tooltip = focused_input(tool_name, input.as_ref())
        .or_else(|| job.and_then(|job| job.command.as_deref()))
        .unwrap_or(&header_text)
        .to_owned();
    soft_frame(palette().SURFACE).show(ui, |ui| {
        let title = sticky::Header::reserve(ui, id);
        for review in reviews {
            review_line(ui, review);
        }
        if let ToolStatus::Drafting { arguments, .. } = status {
            draft_tail(ui, arguments);
        }
        // An expanded card already shows the output in full.
        let preview = context.latest_for_job && (running || !expanded);
        if let Some(job) = job.filter(|job| preview && job.is_running()) {
            live_tail(ui, job);
        }
        if expanded && !running && folded > 1 && shell_control(last).is_some() {
            let combined: String = group
                .iter()
                .filter_map(|entry| match entry {
                    TranscriptEntry::Tool {
                        output: Some(output),
                        ..
                    } => Some(result_body(output)),
                    _ => None,
                })
                .collect();
            ui.label(format!("Output of {folded} polls"));
            code(
                ui,
                if combined.is_empty() {
                    "(no new output)"
                } else {
                    &combined
                },
                palette().TEXT,
            );
        } else if expanded && !running {
            // The patch is what the model asked for; its result stays plain
            // code below, so a rejected patch never reads as applied.
            let patch = input
                .as_ref()
                .filter(|_| tool_name == "apply_patch")
                .and_then(header::patch_text);
            if let Some(patch) = patch {
                ui.label("Patch");
                diff_code(ui, patch);
            } else if let Some(input) = input {
                ui.label("Input");
                let content = focused_input(tool_name, Some(input))
                    .map(Cow::Borrowed)
                    .unwrap_or_else(|| Cow::Owned(pretty_json(input)));
                code(ui, &content, palette().TEXT);
            }
            if let Some(output) = output {
                let is_diff = !*is_error && matches!(tool_name.as_str(), "edit" | "write");
                ui.label(if *is_error {
                    "Error"
                } else if is_diff {
                    "Diff"
                } else {
                    "Output"
                });
                if is_diff {
                    diff_code(ui, output);
                } else {
                    code(
                        ui,
                        &display_output(tool_name, output),
                        if *is_error {
                            palette().ERROR_FG
                        } else {
                            palette().TEXT
                        },
                    );
                }
            }
            if let Some(detail) = detail {
                ui.label("Detail");
                code(
                    ui,
                    &pretty_json(detail),
                    if *is_error {
                        palette().ERROR_FG
                    } else {
                        palette().TEXT
                    },
                );
            }
            if let ToolStatus::Denied { reason } = status {
                code(ui, reason, palette().ERROR_FG);
            }
            if matches!(status, ToolStatus::Interrupted) {
                ui.colored_label(palette().WARNING_FG, INTERRUPTED_NOTE);
            }
        }
        let response = title.show(ui, expanded && !running, |ui| {
            if running {
                crate::theme::widgets::spinner_with(ui, FONT_SMALL, Some(palette().RUNNING));
            }
            let browser_run = run_id.as_deref().filter(|_| {
                crate::panes::agent_browser::is_browser_tool(tool_name)
                    && crate::panes::agent_browser::available(ui.ctx())
            });
            let reserved = if job.is_some() { ROW_DENSE } else { 0.0 }
                + if browser_run.is_some() {
                    VIEW_BUTTON_WIDTH
                } else {
                    0.0
                };
            let width = ui.available_width() - ui.spacing().button_padding.x * 2.0 - reserved;
            let layout = fitted_header_job(ui, glyph, color, &header, width);
            let button =
                ui.add_enabled(!running, egui::Button::new(layout).frame(false).truncate());
            button.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, !running, &label)
            });
            if job.is_some() || browser_run.is_some() {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(job) = job
                        && icon_button(ui, icons::SCROLL, "Open shell job log").clicked()
                    {
                        crate::panes::shell_jobs::request_open(ui.ctx(), &job.key);
                    }
                    if let Some(run) = browser_run
                        && ghost_icon_button(ui, icons::BROWSER, "View")
                            .on_hover_text("Show this run's page in the Browser tab")
                            .clicked()
                    {
                        crate::panes::agent_browser::request_open(ui.ctx(), run);
                    }
                });
            }
            button
                .on_hover_text(&tooltip)
                .on_disabled_hover_text(&tooltip)
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

/// Only states that need attention get a color; finished calls stay muted.
fn status_color(status: &ToolStatus, is_error: bool) -> Color32 {
    match status {
        _ if is_error => palette().ERROR_FG,
        ToolStatus::Running | ToolStatus::Drafting { .. } => palette().INFO,
        ToolStatus::Succeeded | ToolStatus::Approved => palette().TEXT_MUTED,
        ToolStatus::Failed | ToolStatus::Denied { .. } => palette().ERROR_FG,
        ToolStatus::AwaitingApproval => palette().WARNING_FG,
        ToolStatus::Interrupted => palette().WARNING_FG,
    }
}

/// The painted glyph and the accessible name's textual status mark. The
/// painted header only colors the icon and leaves successful calls muted.
fn status_mark(
    status: &ToolStatus,
    is_error: bool,
    icon: &'static str,
) -> (&'static str, &'static str) {
    match status {
        _ if is_error => (icons::X, "✗ "),
        ToolStatus::Running | ToolStatus::Drafting { .. } => ("", ""),
        ToolStatus::Succeeded | ToolStatus::Approved => (icon, "✓ "),
        ToolStatus::Failed | ToolStatus::Denied { .. } => (icons::X, "✗ "),
        ToolStatus::AwaitingApproval => (icons::QUESTION, "? "),
        ToolStatus::Interrupted => (icons::STOP_CIRCLE, "⏹ "),
    }
}

/// Access review outcome, shown whether or not the card is expanded.
fn review_line(ui: &mut Ui, review: &ToolReview) {
    let (icon, color) = if review.needs_attention() {
        (icons::SHIELD_WARNING, palette().WARNING_FG)
    } else {
        (icons::SHIELD_CHECK, palette().TEXT_MUTED)
    };
    let text = review.summary();
    let response = ui.add(
        egui::Label::new(
            RichText::new(icons::with_icon(icon, &text))
                .size(FONT_SMALL)
                .color(color),
        )
        .wrap(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
}

/// The last few lines of a running job, muted, so progress shows without
/// expanding anything.
fn live_tail(ui: &mut Ui, job: &ShellJob) {
    for line in job.tail(LIVE_TAIL_LINES) {
        ui.add(
            egui::Label::new(
                RichText::new(line)
                    .monospace()
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .truncate(),
        );
    }
}

/// Characters of a draft line that are shown; a long one-line JSON keeps its end.
const DRAFT_LINE_CHARS: usize = 120;

/// The newest lines of the arguments being written, each cut to its end.
pub fn draft_preview_lines(arguments: &str) -> Vec<String> {
    let lines: Vec<&str> = arguments.lines().collect();
    let start = lines.len().saturating_sub(LIVE_TAIL_LINES);
    lines[start..]
        .iter()
        .map(|line| {
            let count = line.chars().count();
            if count <= DRAFT_LINE_CHARS {
                return (*line).to_owned();
            }
            let tail: String = line.chars().skip(count - DRAFT_LINE_CHARS).collect();
            format!("…{tail}")
        })
        .collect()
}

/// The last few lines of the arguments the model is still writing, so a long
/// call shows its progress without expanding anything.
fn draft_tail(ui: &mut Ui, arguments: &str) {
    for line in draft_preview_lines(arguments) {
        ui.add(
            egui::Label::new(
                RichText::new(line)
                    .monospace()
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            )
            .truncate(),
        );
    }
}

fn focused_input<'a>(tool_name: &str, input: Option<&'a serde_json::Value>) -> Option<&'a str> {
    let input = input?;
    match tool_name {
        "bash" | "shell" => input.get("command").and_then(serde_json::Value::as_str),
        "read" | "write" | "edit" => ["file_path", "path", "filePath", "file"]
            .iter()
            .find_map(|key| input.get(key).and_then(serde_json::Value::as_str)),
        _ => None,
    }
}

fn display_output<'a>(tool_name: &str, output: &'a str) -> Cow<'a, str> {
    if matches!(tool_name, "bash" | "shell")
        && let Some((exit, streams)) = output.split_once("\n--- stdout ---\n")
        && let Some(code) = exit.strip_prefix("exit_code: ")
        && code.parse::<i32>().is_ok()
        && let Some((stdout, stderr)) = streams.split_once("\n--- stderr ---\n")
    {
        let mut combined = exit.to_owned();
        for stream in [stdout, stderr] {
            if !stream.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(stream);
            }
        }
        return Cow::Owned(combined);
    }
    Cow::Borrowed(output)
}

/// Lays out the header, shortening the location from the left so the
/// subject (file name, command) stays visible in narrow panes.
fn fitted_header_job(
    ui: &Ui,
    glyph: &str,
    color: Color32,
    header: &ToolHeader,
    max_width: f32,
) -> LayoutJob {
    let job_with = |location: Option<&str>| header_job(glyph, color, header, location);
    let Some(location) = header.location.as_deref() else {
        return job_with(None);
    };
    let fits = |location: &str| {
        ui.fonts_mut(|fonts| fonts.layout_job(job_with(Some(location))).size().x) <= max_width
    };
    job_with(Some(&header::shorten_location(location, fits)))
}

/// `<icon> <Verb> <subject> <location> · <meta>`: the subject in body text,
/// everything else muted.
fn header_job(
    glyph: &str,
    color: Color32,
    header: &ToolHeader,
    location: Option<&str>,
) -> LayoutJob {
    let font = FontId::proportional(FONT_BODY);
    let muted = TextFormat::simple(font.clone(), palette().TEXT_MUTED);
    let mut job = LayoutJob::default();
    if !glyph.is_empty() {
        job.append(
            &format!("{glyph} "),
            0.0,
            TextFormat::simple(FontId::proportional(FONT_BODY + 1.0), color),
        );
    }
    let mut verb = muted.clone();
    verb.coords.push("wght", WEIGHT_MEDIUM);
    job.append(&header.verb, 0.0, verb);
    if !header.subject.is_empty() {
        job.append(
            &format!(" {}", header.subject),
            0.0,
            TextFormat::simple(font, palette().TEXT),
        );
    }
    if let Some(location) = location {
        job.append(&format!("  {location}"), 0.0, muted.clone());
    }
    if let Some(meta) = &header.meta {
        job.append(&format!(" · {meta}"), 0.0, muted);
    }
    job
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|error| error.to_string())
}

fn code(ui: &mut Ui, content: &str, color: Color32) {
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(content).monospace().color(color)).wrap());
        });
}

fn diff_code(ui: &mut Ui, content: &str) {
    let mut job = LayoutJob::default();
    for line in content.split_inclusive('\n') {
        let color = if line.starts_with('+') && !line.starts_with("+++ ") {
            palette().SUCCESS
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            palette().ERROR_FG
        } else if line.starts_with("@@") {
            palette().INFO
        } else {
            palette().TEXT
        };
        job.append(
            line,
            0.0,
            TextFormat {
                font_id: FontId::monospace(FONT_SMALL),
                color,
                ..Default::default()
            },
        );
    }
    egui::Frame::new()
        .fill(palette().SURFACE_RAISED)
        .corner_radius(R_SM)
        .inner_margin(SP_2)
        .show(ui, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            });
        });
}
