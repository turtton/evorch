use super::*;
use event_bus::{Event, ToolEvent};
use workbench_protocol::views::{RunPhase, RunRole};
fn models() -> Models {
    let mut m = Models::default();
    for run in ["run", "other"] {
        m.runs.insert(
            run.into(),
            RunSummary {
                id: RunId::new(run).unwrap(),
                thread: ThreadId::new("thread").unwrap(),
                parent_run: None,
                role: RunRole::Worker,
                phase: RunPhase::Running,
                revision: Revision(1),
            },
        );
    }
    m
}
fn start(run: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        run_id: Some(run.into()),
        call_id: "start".into(),
        parent_call_id: None,
        tool_name: "shell".into(),
        input: Some(serde_json::json!({"command": "echo hello"})),
    })
}
fn done(run: &str, uid: &str, status: &str) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        run_id: Some(run.into()),
        call_id: "start".into(),
        parent_call_id: None,
        tool_name: "shell".into(),
        output: Some("job-1 running fake body".into()),
        is_error: false,
        detail: Some(
            serde_json::json!({"shell_job": {"run_id": run, "job_id": "job-1", "job_uid": uid, "status": status, "exit_code": null}}),
        ),
    })
}
fn output(run: &str, uid: &str, offset: u64, chunk: &str, status: &str) -> Event {
    Event::new(ToolEvent::ShellJobOutput {
        run_id: Some(run.into()),
        job_uid: Some(uid.into()),
        job_id: "job-1".into(),
        call_id: Some("start".into()),
        offset,
        chunk: chunk.into(),
        status: status.into(),
        exit_code: (status == "completed").then_some(0),
    })
}
fn read(run: &str, uid: &str) -> ReadShellOutput {
    ReadShellOutput {
        thread: ThreadId::new("thread").unwrap(),
        run: RunId::new(run).unwrap(),
        job_uid: uid.into(),
        offset: 0,
        max_bytes: MAX_SHELL_TAIL_BYTES as u32,
    }
}
fn chunks(events: &[UiEvent]) -> Vec<(u64, &str)> {
    events
        .iter()
        .filter_map(|e| {
            if let UiEvent::Execution(ExecutionEvent::ShellJobOutput { offset, chunk, .. }) = e {
                Some((*offset, chunk.as_str()))
            } else {
                None
            }
        })
        .collect()
}
#[tokio::test(start_paused = true)]
async fn admission_uses_typed_yield_or_output_not_synchronous_result_text() {
    let mut jobs = ShellJobs::default();
    let models = models();
    let now = Instant::now();
    jobs.observe(start("run"), &models, now);
    // Completing within the 10-second auto-yield window exposes neither running
    // detail nor output: duration and result text must not admit this job.
    jobs.observe(done("run", "sync", "completed"), &models, now);
    assert_eq!(jobs.views().count(), 0);
    // A run_js shell can stream even when synchronous. Its forwarder may run
    // after ToolCompleted; output admits it and must retain the observed command.
    jobs.observe(
        output("run", "sync", 0, "late first output", "completed"),
        &models,
        now,
    );
    assert_eq!(
        jobs.views().next().unwrap().command.as_deref(),
        Some("echo hello")
    );
    jobs.clear();
    jobs.observe(start("run"), &models, now);
    assert_eq!(
        jobs.observe(done("run", "yielded", "running"), &models, now)
            .len(),
        1
    );
    let job = jobs.views().next().unwrap();
    assert_eq!(job.command.as_deref(), Some("echo hello"));
    assert_eq!(job.source_call_id.as_deref(), Some("start"));
    assert_eq!(job.state, ShellJobState::Running);
    assert!(jobs.read(&read("run", "yielded")).unwrap().chunk.is_empty());
    jobs.observe(
        output("other", "yielded", 0, "other run", "running"),
        &models,
        now,
    );
    assert_eq!(jobs.views().count(), 2); // identical handle AND uid, distinct run
    assert_eq!(
        jobs.read(&read("other", "yielded")).unwrap().chunk,
        "other run"
    );
    let mut wrong = read("other", "yielded");
    wrong.thread = ThreadId::new("not-owner").unwrap();
    assert_eq!(jobs.read(&wrong).unwrap_err().code, ErrorCode::NotFound);
    jobs.observe(
        output("unknown", "uid", 0, "hidden", "running"),
        &models,
        now,
    );
    assert_eq!(jobs.views().count(), 2);
}
#[tokio::test(start_paused = true)]
async fn volume_is_bounded_rate_limited_and_final_tail_flushes_without_new_input() {
    let mut jobs = ShellJobs::default();
    let mut models = models();
    let mut events = Vec::new();
    let mut offset = 0;
    let chunk = "界".repeat(4096);
    for _ in 0..1024 {
        events.extend(jobs.observe(
            output("run", "uid", offset, &chunk, "running"),
            &models,
            Instant::now(),
        ));
        offset += chunk.len() as u64;
        assert!(jobs.jobs.values().next().unwrap().tail.len() <= MAX_SHELL_TAIL_BYTES);
    }
    assert_eq!(chunks(&events).len(), 1);
    assert!(jobs.flush(Instant::now()).is_empty());
    tokio::time::advance(Duration::from_millis(249)).await;
    assert!(jobs.flush(Instant::now()).is_empty());
    let terminal = jobs.observe(
        output("run", "uid", offset, "last", "completed"),
        &models,
        Instant::now(),
    );
    assert!(
        matches!(&terminal[..], [UiEvent::Execution(ExecutionEvent::ShellJobChanged { job })] if job.state == ShellJobState::Completed && job.exit_code == Some(0))
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    events.extend(jobs.flush(Instant::now()));
    let output = chunks(&events);
    assert_eq!(output.len(), 2);
    assert!(
        output
            .iter()
            .all(|(_, chunk)| chunk.len() <= MAX_SHELL_OUTPUT_EVENT_BYTES)
    );
    assert!(output[1].0 > output[0].0 + output[0].1.len() as u64);
    assert!(output[1].1.ends_with("last"));
    assert!(jobs.flush(Instant::now()).is_empty());
    let read = jobs.read(&read("run", "uid")).unwrap();
    assert!(read.chunk.len() <= MAX_SHELL_TAIL_BYTES);
    assert!(read.chunk.ends_with("last"));
    assert!(read.available_from > 0);
    models.runs.get_mut("run").unwrap().phase = RunPhase::Done;
    jobs.observe(done("run", "uid", "running"), &models, Instant::now());
    assert_eq!(jobs.views().next().unwrap().state, ShellJobState::Completed);
}
#[tokio::test(start_paused = true)]
async fn duplicate_overlap_gap_and_utf8_offsets_have_one_bounded_tail() {
    let mut jobs = ShellJobs::default();
    let models = models();
    let now = Instant::now();
    for (offset, chunk) in [(0, "abc"), (0, "abc"), (1, "bcde"), (10, "界z")] {
        jobs.observe(output("run", "uid", offset, chunk, "running"), &models, now);
    }
    let mut q = read("run", "uid");
    let result = jobs.read(&q).unwrap();
    assert_eq!(
        (
            result.available_from,
            result.offset,
            result.output_end,
            result.chunk.as_str()
        ),
        (10, 10, 14, "界z")
    );
    q.offset = 11;
    assert_eq!(jobs.read(&q).unwrap().offset, 13);
    q.offset = 10;
    q.max_bytes = 3;
    assert_eq!(jobs.read(&q).unwrap().chunk, "界");
    jobs.observe(
        output("run", "uid", u64::MAX, "invalid", "running"),
        &models,
        now,
    );
    assert_eq!(jobs.read(&read("run", "uid")).unwrap().chunk, "界z");
}
#[tokio::test(start_paused = true)]
async fn terminal_count_is_bounded_and_run_end_interrupts_uncertain_jobs() {
    let mut jobs = ShellJobs::default();
    let models = models();
    let now = Instant::now();
    jobs.observe(output("run", "active", 0, "", "running"), &models, now);
    for index in 0..MAX_RETAINED_SHELL_JOBS + 5 {
        jobs.observe(
            output("other", &format!("uid-{index}"), 0, "tail", "completed"),
            &models,
            now,
        );
    }
    assert_eq!(jobs.views().count(), MAX_RETAINED_SHELL_JOBS + 1);
    assert!(jobs.read(&read("other", "uid-0")).is_err());
    assert!(jobs.read(&read("run", "active")).is_ok());
    let terminal = jobs.observe(
        Event::new(BusLifecycle::AgentRunStateChanged {
            run_id: "run".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
            waiting: None,
        }),
        &models,
        now,
    );
    assert!(
        matches!(&terminal[..], [UiEvent::Execution(ExecutionEvent::ShellJobChanged { job })] if job.state == ShellJobState::Interrupted)
    );
    assert_eq!(jobs.views().count(), MAX_RETAINED_SHELL_JOBS);
    jobs.clear();
    assert_eq!(jobs.views().count(), 0);
    assert!(jobs.starts.is_empty());
}

#[tokio::test(start_paused = true)]
async fn recent_command_correlation_is_bounded_without_admitting_synchronous_jobs() {
    let mut jobs = ShellJobs::default();
    let models = models();
    let now = Instant::now();
    for index in 0..MAX_RETAINED_SHELL_JOBS + 5 {
        for mut event in [
            start("run"),
            done("run", &format!("uid-{index}"), "completed"),
        ] {
            match &mut event.kind {
                EventKind::Tool(
                    ToolEvent::ToolStarted { call_id, .. }
                    | ToolEvent::ToolCompleted { call_id, .. },
                ) => *call_id = format!("call-{index}"),
                _ => unreachable!(),
            }
            jobs.observe(event, &models, now);
        }
    }
    assert_eq!(jobs.views().count(), 0);
    assert_eq!(jobs.starts.len(), MAX_RETAINED_SHELL_JOBS);
    assert_eq!(jobs.completed_starts.len(), MAX_RETAINED_SHELL_JOBS);
    jobs.interrupt("run");
    assert!(jobs.starts.is_empty());
    assert!(jobs.completed_starts.is_empty());
}
