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
        job_uid: None,
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
        gui::panes::shell_jobs::take_open_request(&harness.ctx),
        Some(gui::model::transcript::shell_jobs::ShellJobKey::new(
            None, JOB, None
        ))
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

#[test]
fn short_shell_job_handle_joins_result_and_live_output_in_one_card() {
    for live_first in [false, true] {
        let mut model = TranscriptModel::new();
        let live = Event::new(ToolEvent::ShellJobOutput {
            job_uid: None,
            job_id: "job-0".into(),
            call_id: Some("short-start".into()),
            run_id: None,
            offset: 0,
            chunk: "short handle output\n".into(),
            status: "running".into(),
            exit_code: None,
        });
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "short-start".into(),
            input: Some(json!({"command":"cargo test", "yield_ms":0})),
            run_id: None,
        }));
        if live_first {
            model.apply(&live);
        }
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "short-start".into(),
            is_error: false,
            output: Some("shell job: job-0\nstatus: running\ncursor: 0\n".into()),
            detail: Some(json!({"shell_job":{"job_id":"job-0", "status":"running"}})),
            run_id: None,
        }));
        if !live_first {
            model.apply(&live);
        }
        assert_eq!(model.shell_jobs().iter().count(), 1);
        let job = model.shell_jobs().get("job-0").unwrap();
        assert_eq!(job.command.as_deref(), Some("cargo test"));
        assert_eq!(job.log(), "short handle output\n");
        let mut harness = harness(model);
        assert_eq!(
            harness
                .query_all_by_label("✓ Shell cargo test · running")
                .count(),
            1
        );
        assert_eq!(harness.query_all_by_label("short handle output").count(), 1);
        harness.get_by_label("Open shell job log").click();
        harness.run_steps(1);
        assert_eq!(
            gui::panes::shell_jobs::take_open_request(&harness.ctx),
            Some(gui::model::transcript::shell_jobs::ShellJobKey::new(
                None, "job-0", None
            ))
        );
    }
}

/// The call ID and short handle may both be identical across owners.
#[test]
fn scoped_cards_keep_commands_logs_folding_and_open_requests_separate() {
    use gui::model::transcript::shell_jobs::ShellJobKey;
    use gui::panes::transcript_tool::card_len;
    for same_run in [false, true] {
        for live_first in [false, true] {
            let mut model = TranscriptModel::new();
            let owners = if same_run {
                ["run-a", "run-a"]
            } else {
                ["run-a", "run-b"]
            };
            // Includes a legacy historical result with no UUID.
            let uids = [None, Some("internal-b")];
            for (index, (run, uid)) in owners.into_iter().zip(uids).enumerate() {
                let command = format!("command-{index}");
                let live = Event::new(ToolEvent::ShellJobOutput {
                    job_uid: uid.map(str::to_owned),
                    job_id: "job-0".into(),
                    call_id: Some("start".into()),
                    run_id: Some(run.into()),
                    offset: 0,
                    chunk: format!("log-{index}\n"),
                    status: "running".into(),
                    exit_code: None,
                });
                if live_first {
                    model.apply(&live);
                }
                model.apply(&Event::new(ToolEvent::ToolStarted {
                    tool_name: "shell".into(),
                    call_id: "start".into(),
                    input: Some(json!({"command": command})),
                    run_id: Some(run.into()),
                }));
                model.apply(&Event::new(ToolEvent::ToolCompleted {
                    tool_name: "shell".into(), call_id: "start".into(), is_error: false,
                    output: Some("poll fallback\n".into()),
                    detail: Some(json!({"shell_job": {"job_id": "job-0", "job_uid": uid, "status": "running"}})),
                    run_id: Some(run.into()),
                }));
                if !live_first {
                    model.apply(&live);
                }
                let key = ShellJobKey::new(Some(run), "job-0", uid);
                let job = model.shell_jobs().get_key(&key).unwrap();
                assert_eq!(job.command.as_deref(), Some(command.as_str()));
                assert_eq!(job.log(), format!("log-{index}\n"));
            }
            assert_eq!(model.shell_jobs().iter().count(), 2);
            assert!(model.shell_jobs().get("job-0").is_none());
            let mut entries = model.entries().to_vec();
            for entry in &mut entries {
                if let gui::model::transcript::TranscriptEntry::Tool { input, .. } = entry {
                    *input = Some(json!({"action": "poll", "job_id": "job-0"}));
                }
            }
            assert_eq!(
                card_len(&entries, model.shell_jobs()),
                1,
                "distinct job instances must not fold"
            );
            let mut h = harness(model.clone());
            assert!(h.query_by_label("log-0").is_some());
            assert!(h.query_by_label("log-1").is_some());
            let buttons: Vec<_> = h.query_all_by_label("Open shell job log").collect();
            assert_eq!(buttons.len(), 2);
            buttons[0].click();
            h.run_steps(1);
            let key = gui::panes::shell_jobs::take_open_request(&h.ctx).unwrap();
            assert_eq!(key, ShellJobKey::new(Some(owners[0]), "job-0", uids[0]));
            let mut pane = gui::panes::shell_jobs::ShellJobsPane::default();
            pane.select(key);
            let mut h = Harness::new_ui(move |ui| {
                gui::theme::install(ui.ctx());
                pane.render(ui, &model.shell_jobs().iter().collect::<Vec<_>>());
            });
            h.run_steps(2);
            assert!(h.query_by_label("command-0").is_some());
            assert!(h.query_by_label("command-1").is_some());
            assert!(h.query_by_label("$ command-0").is_some());
            assert!(h.query_by_label("log-0").is_some());
            h.get_by_label("command-1").click();
            h.run_steps(2);
            assert!(h.query_by_label("$ command-1").is_some());
            assert!(h.query_by_label("log-1").is_some());
            assert!(h.query_by_label("log-0").is_none());
        }
    }
}

fn scoped_job(model: &mut TranscriptModel, run: &str, uid: &str) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "start".into(),
        input: Some(json!({"command": format!("command-{run}")})),
        run_id: Some(run.into()),
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "start".into(),
        is_error: false,
        output: None,
        detail: Some(json!({"shell_job": {"job_id": JOB, "job_uid": uid, "status": "running"}})),
        run_id: Some(run.into()),
    }));
    model.apply(&Event::new(ToolEvent::ShellJobOutput {
        job_uid: Some(uid.into()),
        job_id: JOB.into(),
        call_id: Some("start".into()),
        run_id: Some(run.into()),
        offset: 0,
        chunk: format!("output-{run}\n"),
        status: "running".into(),
        exit_code: None,
    }));
}

#[test]
fn unfinished_controls_resolve_the_owned_job_for_headers_previews_and_open_requests() {
    use gui::model::transcript::shell_jobs::ShellJobKey;
    for (action, verb) in [("poll", "Poll"), ("stdin", "Input"), ("stop", "Stop")] {
        let mut model = TranscriptModel::new();
        scoped_job(&mut model, "run-a", "internal-a");
        scoped_job(&mut model, "run-b", "internal-b");
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "control".into(),
            input: Some(json!({"action": action, "job_id": JOB})),
            run_id: Some("run-a".into()),
        }));
        let mut h = harness(model);
        assert!(
            h.query_by_label(&format!("{verb} command-run-a #3f9c2a1e · running"))
                .is_some()
        );
        assert_eq!(h.query_all_by_label("output-run-a").count(), 1);
        assert_eq!(h.query_all_by_label("output-run-b").count(), 1);
        let buttons: Vec<_> = h.query_all_by_label("Open shell job log").collect();
        assert_eq!(buttons.len(), 3);
        buttons[2].click();
        h.run_steps(1);
        assert_eq!(
            gui::panes::shell_jobs::take_open_request(&h.ctx),
            Some(ShellJobKey::new(Some("run-a"), JOB, Some("internal-a")))
        );
    }
}

#[test]
fn unfinished_poll_folds_with_the_previous_poll_before_and_after_completion() {
    let mut model = TranscriptModel::new();
    scoped_job(&mut model, "run-a", "internal-a");
    let completed = |call: &str| {
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: call.into(),
            is_error: false,
            output: Some(format!("{call} output\n")),
            detail: Some(
                json!({"shell_job": {"job_id": JOB, "job_uid": "internal-a", "status": "running"}}),
            ),
            run_id: Some("run-a".into()),
        })
    };
    for call in ["poll-1", "poll-2"] {
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: call.into(),
            input: Some(json!({"action": "poll", "job_id": JOB})),
            run_id: Some("run-a".into()),
        }));
        if call == "poll-1" {
            model.apply(&completed(call));
        }
    }
    let mut h = Harness::new_ui_state(
        |ui, model| {
            gui::theme::install(ui.ctx());
            gui::panes::agent::transcript_body(ui, model);
        },
        model,
    );
    h.run_steps(2);
    assert!(
        h.query_by_label("Poll command-run-a #3f9c2a1e · ×2 · running")
            .is_some()
    );
    assert_eq!(h.query_all_by_label("output-run-a").count(), 1);
    h.state_mut().apply(&completed("poll-2"));
    h.run_steps(2);
    h.get_by_label("✓ Poll command-run-a #3f9c2a1e · ×2 · running")
        .click();
    h.run_steps(2);
    assert!(h.query_by_label("Output of 2 polls").is_some());
    assert!(
        h.query_by_label_contains("poll-1 output\npoll-2 output")
            .is_some()
    );
}

#[test]
fn pending_controls_refuse_ambiguous_jobs_and_completed_legacy_calls_keep_exact_identity() {
    use gui::panes::transcript_tool::resolved_shell_job_key;
    let mut model = TranscriptModel::new();
    scoped_job(&mut model, "run-a", "internal-a");
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "legacy-poll".into(),
        input: Some(json!({"action": "poll", "job_id": JOB})),
        run_id: Some("run-a".into()),
    }));
    // A completed legacy call without a result UUID must never borrow the
    // modern job's UUID, even when there is only one matching job in store.
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "legacy-poll".into(),
        is_error: false,
        output: None,
        detail: None,
        run_id: Some("run-a".into()),
    }));
    let key = resolved_shell_job_key(model.entries().last().unwrap(), model.shell_jobs()).unwrap();
    assert!(key.uid.is_none());
    assert!(model.shell_jobs().get_key(&key).is_none());
    scoped_job(&mut model, "run-a", "restored-instance");
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "pending-poll".into(),
        input: Some(json!({"action": "poll", "job_id": JOB})),
        run_id: Some("run-a".into()),
    }));
    assert!(resolved_shell_job_key(model.entries().last().unwrap(), model.shell_jobs()).is_none());
    let h = harness(model);
    assert!(h.query_by_label("Poll #3f9c2a1e").is_some());
    assert!(h.query_by_label("✓ Poll #3f9c2a1e").is_some());
    assert_eq!(h.query_all_by_label("Open shell job log").count(), 2);
}

#[test]
fn expanded_card_keeps_its_state_when_job_identity_arrives() {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "start".into(),
        input: Some(json!({"command": "cargo test"})),
        run_id: Some("run-a".into()),
    }));
    let completed = |detail| {
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "start".into(),
            is_error: false,
            output: Some("returned output".into()),
            detail,
            run_id: Some("run-a".into()),
        })
    };
    model.apply(&completed(None));
    let mut h = Harness::new_ui_state(
        |ui, model| {
            gui::theme::install(ui.ctx());
            gui::panes::agent::transcript_body(ui, model);
        },
        model,
    );
    h.run_steps(2);
    h.get_by_label("✓ Shell cargo test").click();
    h.run_steps(2);
    assert!(h.query_by_label("returned output").is_some());
    h.state_mut().apply(&completed(Some(json!({"shell_job": {
        "job_id": JOB, "job_uid": "internal-a", "status": "running"
    }}))));
    h.run_steps(2);
    assert!(h.query_by_label("returned output").is_some());
    assert!(h.query_by_label("Detail").is_some());
}

#[test]
fn counter_handles_remain_distinct_in_control_cards_and_the_shell_jobs_pane() {
    let mut model = TranscriptModel::new();
    for handle in ["job-10000", "job-10001"] {
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: handle.into(),
            input: Some(json!({"action": "poll", "job_id": handle})),
            run_id: Some("run-a".into()),
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: handle.into(),
            is_error: false,
            output: None,
            detail: Some(json!({"shell_job": {"job_id": handle, "status": "running"}})),
            run_id: Some("run-a".into()),
        }));
    }
    let cards = harness(model.clone());
    for handle in ["job-10000", "job-10001"] {
        assert!(
            cards
                .query_by_label(&format!("✓ Poll #{handle} · running"))
                .is_some()
        );
    }
    let mut pane = gui::panes::shell_jobs::ShellJobsPane::default();
    let mut h = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        pane.render(ui, &model.shell_jobs().iter().collect::<Vec<_>>());
    });
    h.run_steps(2);
    for handle in ["job-10000", "job-10001"] {
        assert!(h.query_by_label(&format!("{handle} · running")).is_some());
        // Poll-only restored history has no command, so the selectable row
        // also needs the full handle to identify the requested log.
        assert!(h.query_by_label(&format!("job {handle}")).is_some());
    }
}
