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
    events_for_job(JOB)
}

fn events_for_job(job: &str) -> Vec<Event> {
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
            output: Some(format!("shell job: {job}\nstatus: running\ncursor: 0\n")),
            detail: Some(json!({"shell_job": {"job_id": job, "status": "running"}})),
            run_id: None,
        }),
        Event::new(ToolEvent::ShellJobOutput {
            job_uid: None,
            job_id: job.into(),
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

#[test]
fn short_shell_job_handle_opens_the_combined_result_and_live_log() {
    let mut harness = workbench();
    harness.state_mut().apply_events(events_for_job("job-12"));
    harness.run();
    assert_eq!(
        harness.count_labels("✓ Shell cargo nextest run · running"),
        1
    );
    assert_eq!(harness.count_labels("Open shell job log"), 1);
    assert!(harness.has_label("        PASS shell_job_cards"));
    harness.click_label("Open shell job log");
    harness.run();
    harness.run();
    assert_eq!(harness.count_labels("$ cargo nextest run"), 1);
    assert!(harness.count_labels("        PASS shell_job_cards") >= 2);
    assert!(harness.has_label("Follow"));
}

#[test]
fn same_handle_in_two_subagent_runs_is_visible_and_opens_the_right_log() {
    use gui::model::transcript::shell_jobs::ShellJobKey;
    use workspace_ui::{ProjectId, SidebarState, ThreadId};
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "thread")
        .unwrap();
    sidebar.threads[0].run_ids = vec!["run-a".into(), "run-b".into()];
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    let state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar);
    let mut gui = gui::headless::HeadlessWorkbench::new(state, [1400.0, 1000.0]);
    for (index, run) in ["run-a", "run-b"].into_iter().enumerate() {
        gui.state_mut().apply_events([
            Event::new(ToolEvent::ToolStarted {
                tool_name: "shell".into(),
                call_id: "start".into(),
                input: Some(json!({"command": format!("command-{index}")})),
                run_id: Some(run.into()),
            }),
            Event::new(ToolEvent::ToolCompleted {
                tool_name: "shell".into(),
                call_id: "start".into(),
                is_error: false,
                output: Some("returned output\n".into()),
                detail: Some(json!({"shell_job": {"job_id": "job-0", "status": "running"}})),
                run_id: Some(run.into()),
            }),
            Event::new(ToolEvent::ShellJobOutput {
                job_uid: None,
                job_id: "job-0".into(),
                call_id: Some("start".into()),
                run_id: Some(run.into()),
                offset: 0,
                chunk: format!("owner-{index}-output\n"),
                status: "running".into(),
                exit_code: None,
            }),
        ]);
    }
    for (index, run) in ["run-a", "run-b"].into_iter().enumerate() {
        gui.state_mut()
            .open_shell_jobs_tab(ShellJobKey::new(Some(run), "job-0", None));
        gui.run();
        for command in ["command-0", "command-1"] {
            assert!(
                gui.has_label(command),
                "both same-handle rows must survive dedup"
            );
        }
        assert!(gui.has_label(&format!("$ command-{index}")));
        assert!(gui.has_label(&format!("owner-{index}-output")));
        assert!(!gui.has_label(&format!("owner-{}-output", 1 - index)));
    }
}
