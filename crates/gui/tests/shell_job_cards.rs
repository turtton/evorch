use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;
use serde_json::json;

const JOB: &str = "3f9c2a1e-0000-4000-8000-000000000000";

fn tool(
    model: &mut TranscriptModel,
    call: &str,
    input: serde_json::Value,
    output: &str,
    status: &str,
) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: call.into(),
        input: Some(input),
        run_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output.into()),
        detail: Some(json!({"shell_job": {"job_id": JOB, "status": status}})),
        run_id: None,
    }));
}

fn started() -> TranscriptModel {
    let mut model = TranscriptModel::new();
    tool(
        &mut model,
        "start",
        json!({"command": "cargo test", "yield_ms": 0}),
        &format!("shell job: {JOB}\nstatus: running\ncursor: 0\n"),
        "running",
    );
    model
}

fn poll(model: &mut TranscriptModel, call: &str, body: &str) {
    tool(
        model,
        call,
        json!({"action": "poll", "job_id": JOB}),
        &format!("shell job: {JOB}\nstatus: running\ncursor: 1\n{body}"),
        "running",
    );
}

fn live(model: &mut TranscriptModel, offset: u64, chunk: &str, status: &str) {
    model.apply(&Event::new(ToolEvent::ShellJobOutput {
        job_id: JOB.into(),
        call_id: Some("start".into()),
        run_id: None,
        offset,
        chunk: chunk.into(),
        status: status.into(),
        exit_code: None,
    }));
}

fn harness(model: TranscriptModel) -> Harness<'static> {
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent::transcript_body(ui, &model);
    });
    harness.run_steps(2);
    harness
}

#[test]
fn consecutive_polls_fold_into_one_card_named_by_the_command() {
    let mut model = started();
    poll(&mut model, "poll-1", "compiling\n");
    poll(&mut model, "poll-2", "");
    poll(&mut model, "poll-3", "test result: ok\n");
    let mut harness = harness(model);
    let card = "✓ Poll cargo test #3f9c2a1e · ×3 · running";
    assert!(harness.query_by_label(card).is_some());
    assert!(harness.query_by_label_contains("×2").is_none());
    harness.get_by_label(card).click();
    harness.run_steps(3);
    assert!(harness.query_by_label("Output of 3 polls").is_some());
    assert!(
        harness
            .query_by_label_contains("compiling\ntest result: ok")
            .is_some()
    );
    if let Some(directory) = std::env::var_os("EVORCH_TOOL_CAPTURE_DIR") {
        harness
            .render()
            .expect("render folded polls")
            .save(std::path::Path::new(&directory).join("tool-shell-polls-folded.png"))
            .expect("save capture");
    }
}

#[test]
fn a_poll_after_other_activity_starts_a_new_card() {
    let mut model = started();
    poll(&mut model, "poll-1", "");
    model.push_notice("between");
    poll(&mut model, "poll-2", "");
    let harness = harness(model);
    assert_eq!(
        harness
            .query_all_by_label("✓ Poll cargo test #3f9c2a1e · running")
            .count(),
        2
    );
}

#[test]
fn only_the_latest_card_previews_live_output_and_opens_the_log() {
    let mut model = started();
    live(&mut model, 0, "first line\nsecond line\n", "running");
    poll(&mut model, "poll-1", "first line\n");
    let mut harness = harness(model);
    assert_eq!(harness.query_all_by_label("second line").count(), 1);
    let buttons: Vec<_> = harness.query_all_by_label("Open shell job log").collect();
    assert_eq!(buttons.len(), 2);
    buttons[1].click();
    harness.run_steps(1);
    assert_eq!(
        gui::panes::shell_jobs::take_open_request(&harness.ctx).as_deref(),
        Some(JOB)
    );
}

#[test]
fn finished_jobs_stop_previewing_output() {
    let mut model = started();
    live(&mut model, 0, "done\n", "completed");
    let harness = harness(model);
    assert!(harness.query_by_label("done").is_none());
    assert!(
        harness
            .query_by_label("✓ Shell cargo test · completed")
            .is_some()
    );
}

#[test]
fn shell_jobs_pane_lists_jobs_and_shows_the_selected_log() {
    let mut model = started();
    live(&mut model, 0, "alpha\nbeta\n", "running");
    let mut pane = gui::panes::shell_jobs::ShellJobsPane::default();
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        let jobs: Vec<_> = model.shell_jobs().iter().collect();
        pane.render(ui, &jobs);
    });
    harness.run_steps(2);
    assert!(harness.query_by_label("cargo test").is_some());
    assert!(harness.query_by_label("$ cargo test").is_some());
    assert!(harness.query_by_label("beta").is_some());
    assert!(harness.query_by_label("Follow").is_some());
}

#[test]
fn shell_jobs_pane_explains_an_empty_conversation() {
    let mut pane = gui::panes::shell_jobs::ShellJobsPane::default();
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        pane.render(ui, &[]);
    });
    harness.run_steps(2);
    assert!(harness.query_by_label("No shell jobs").is_some());
}
