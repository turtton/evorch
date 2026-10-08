//! A shell job card opens the Shell jobs tab on that job's live log.

use event_bus::{Event, ToolEvent};
use gui::fixture::DemoSource;
use serde_json::json;

const JOB: &str = "3f9c2a1e-0000-4000-8000-000000000000";

fn workbench() -> gui::headless::HeadlessWorkbench<DemoSource> {
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .expect("workbench");
    gui::headless::HeadlessWorkbench::new(state, [1400.0, 1000.0])
}

fn job_events() -> Vec<Event> {
    vec![
        Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "start".into(),
            input: Some(json!({"command": "cargo nextest run", "yield_ms": 0})),
            run_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "start".into(),
            is_error: false,
            output: Some(format!("shell job: {JOB}\nstatus: running\ncursor: 0\n")),
            detail: Some(json!({"shell_job": {"job_id": JOB, "status": "running"}})),
            run_id: None,
        }),
        Event::new(ToolEvent::ShellJobOutput {
            job_id: JOB.into(),
            call_id: Some("start".into()),
            run_id: None,
            offset: 0,
            chunk: "   Compiling gui v0.1.0\n    Starting 18 tests\n        PASS shell_job_cards\n"
                .into(),
            status: "running".into(),
            exit_code: None,
        }),
    ]
}

#[test]
fn job_card_opens_the_shell_jobs_tab_on_its_log() {
    let mut harness = workbench();
    harness.state_mut().apply_events(job_events());
    harness.run();
    assert!(
        harness.has_label("✓ Shell cargo nextest run · running"),
        "card"
    );
    assert!(harness.has_label("        PASS shell_job_cards"));
    assert!(!harness.has_label("$ cargo nextest run"));

    harness.click_label("Open shell job log");
    harness.run();
    harness.run();

    assert!(harness.has_label("$ cargo nextest run"));
    assert!(harness.has_label("Follow"));
    assert!(harness.count_labels("        PASS shell_job_cards") >= 2);
    if let Some(directory) = std::env::var_os("EVORCH_SHELL_JOBS_CAPTURE_DIR") {
        harness
            .capture()
            .expect("render workbench")
            .save_png(&std::path::Path::new(&directory).join("shell-jobs.png"))
            .expect("save capture");
    }
}
