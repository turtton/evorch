use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

// Drive the real wait/control paths without child-process scheduling or
// wall-clock deadlines; process teardown is covered by job_tests.
fn fixture() -> (JobRegistry, Arc<Job>, mpsc::Receiver<Input>) {
    let (changed, _) = watch::channel(0);
    let (cancel, _) = watch::channel(false);
    let (input, received) = mpsc::channel(4);
    let job = Arc::new(Job {
        id: "internal-job-id".into(),
        handle: "job-0".into(),
        owner: "owner".into(),
        call_id: None,
        thread: None,
        synchronous: false,
        stream_output: false,
        timeout_ms: None,
        state: Mutex::new(State {
            output: LiveOutput::default(),
            final_result: None,
            exposed: true,
            completion: None,
            observed: false,
            completion_notified: false,
            artifact: None,
            artifact_notice: None,
            process: None,
            sandbox: None,
            delivered_cursor: 0,
            last_seen: None,
        }),
        changed,
        cancel,
        input,
        escalated: None,
    });
    let registry = JobRegistry::default();
    registry
        .jobs
        .lock()
        .unwrap()
        .insert(job.id.clone(), Arc::clone(&job));
    registry
        .by_handle
        .lock()
        .unwrap()
        .insert(job.handle.clone(), Arc::clone(&job));
    (registry, job, received)
}

fn owner() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "owner".into(),
        thread_id: None,
        call_id: None,
        ..Default::default()
    }
}

fn args(action: &str, yield_ms: u64) -> ControlArgs {
    serde_json::from_value(serde_json::json!({
        "action": action, "job_id": "job-0", "yield_ms": yield_ms,
        "input": (action == "stdin").then_some("hello\n")
    }))
    .unwrap()
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

async fn ready<F: Future>(mut future: Pin<&mut F>) -> F::Output {
    std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(value) => Poll::Ready(value),
        Poll::Pending => panic!("wait must return before any further time advance"),
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn start_wait_ignores_output_and_keeps_its_original_deadline() {
    for yield_ms in [1000, MAX_WAIT_YIELD_MS] {
        let (_registry, job, _input) = fixture();
        job.push(0, b"ready\n");
        let mut waiting = std::pin::pin!(job.wait(yield_ms, None));
        pending(waiting.as_mut()).await;
        tokio::time::advance(Duration::from_millis(yield_ms - 1)).await;
        job.push(0, b"more\n");
        pending(waiting.as_mut()).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        ready(waiting.as_mut()).await;
        assert!(job.running());
        assert!(job.snapshot(0).0.content.contains("ready\nmore\n"));
    }
}

#[tokio::test(start_paused = true)]
async fn every_terminal_status_ends_start_wait_early() {
    for status in ["completed", "failed", "timed_out", "cancelled"] {
        let (_registry, job, _input) = fixture();
        let mut waiting = std::pin::pin!(job.wait(60_000, None));
        pending(waiting.as_mut()).await;
        job.push(0, b"partial\n");
        pending(waiting.as_mut()).await;
        job.complete(Completion {
            status,
            exit_code: None,
            error: None,
        });
        ready(waiting.as_mut()).await;
        let mut already_finished = std::pin::pin!(job.wait(60_000, None));
        ready(already_finished.as_mut()).await;
        assert_eq!(
            job.snapshot(0).0.detail.unwrap()["shell_job"]["status"],
            status
        );
    }
}

#[tokio::test(start_paused = true)]
async fn stop_wait_ignores_output_and_returns_on_teardown_or_deadline() {
    for complete in [false, true] {
        let (registry, job, _input) = fixture();
        let owner = owner();
        job.push(0, b"before stop\n");
        let mut stopping = std::pin::pin!(registry.control(&owner, args("stop", 1000)));
        pending(stopping.as_mut()).await;
        assert!(*job.cancel.borrow());
        tokio::time::advance(Duration::from_millis(500)).await;
        job.push(0, b"after stop\n");
        pending(stopping.as_mut()).await;
        if complete {
            job.complete(Completion {
                status: "cancelled",
                exit_code: None,
                error: None,
            });
        } else {
            tokio::time::advance(Duration::from_millis(500)).await;
        }
        let result = ready(stopping.as_mut()).await.unwrap();
        assert_eq!(
            result.detail.unwrap()["shell_job"]["status"],
            if complete { "cancelled" } else { "running" }
        );
        assert!(result.content.contains("before stop\nafter stop\n"));
    }
}

#[tokio::test(start_paused = true)]
async fn zero_start_and_zero_or_omitted_stop_return_immediately() {
    let (registry, job, _input) = fixture();
    let mut starting = std::pin::pin!(job.wait(0, None));
    ready(starting.as_mut()).await;
    let owner = owner();
    for value in [
        serde_json::json!({"action": "stop", "job_id": "job-0", "yield_ms": 0}),
        serde_json::json!({"action": "stop", "job_id": "job-0"}),
    ] {
        let mut stopping =
            std::pin::pin!(registry.control(&owner, serde_json::from_value(value).unwrap()));
        let result = ready(stopping.as_mut()).await.unwrap();
        assert_eq!(result.detail.unwrap()["shell_job"]["status"], "running");
        assert!(*job.cancel.borrow());
    }
}

#[tokio::test(start_paused = true)]
async fn poll_and_stdin_still_return_early_on_output() {
    for action in ["poll", "stdin"] {
        let (registry, job, mut input) = fixture();
        let owner = owner();
        let mut arguments = args(action, 60_000);
        if action == "poll" {
            arguments.return_on = ReturnOn::Output;
        }
        let mut waiting = std::pin::pin!(registry.control(&owner, arguments));
        pending(waiting.as_mut()).await;
        if action == "stdin" {
            let request = input.recv().await.unwrap();
            assert_eq!(request.bytes, b"hello\n");
            request.result.send(Ok(())).unwrap();
            pending(waiting.as_mut()).await;
        }
        job.push(0, b"new output\n");
        let result = ready(waiting.as_mut()).await.unwrap();
        assert_eq!(result.detail.unwrap()["shell_job"]["status"], "running");
        assert!(result.content.contains("new output\n"));
    }
}

async fn control(registry: &JobRegistry, value: serde_json::Value) -> ToolResult {
    let owner = owner();
    let mut call = std::pin::pin!(registry.control(&owner, serde_json::from_value(value).unwrap()));
    ready(call.as_mut()).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn omitted_cursor_continues_after_returned_output_and_explicit_cursor_rereads() {
    let (registry, job, _input) = fixture();
    job.push(0, b"first\n");
    // The start response already returned the first line.
    assert!(job.snapshot(0).0.content.contains("first\n"));
    job.push(0, b"second\n");
    let poll = serde_json::json!({"action": "poll", "job_id": "job-0", "return_on":"output"});
    let next = control(&registry, poll.clone()).await;
    assert!(next.content.contains("second\n"));
    assert!(!next.content.contains("first\n"));
    let reread = control(
        &registry,
        serde_json::json!({"action": "poll", "job_id": "job-0", "cursor": 0, "return_on":"output"}),
    )
    .await;
    assert!(reread.content.contains("first\nsecond\n"));
    // A re-read does not move the continuation point backwards.
    job.push(0, b"third\n");
    let later = control(&registry, poll).await;
    assert!(later.content.contains("third\n"));
    assert!(!later.content.contains("first\n") && !later.content.contains("second\n"));
}

#[tokio::test(start_paused = true)]
async fn omitted_cursor_poll_waits_for_output_after_the_returned_cursor() {
    let (registry, job, _input) = fixture();
    job.push(0, b"old\n");
    let _ = job.snapshot(0);
    let owner = owner();
    let mut arguments = args("poll", 60_000);
    arguments.return_on = ReturnOn::Output;
    let mut waiting = std::pin::pin!(registry.control(&owner, arguments));
    // Output already returned does not end the wait.
    pending(waiting.as_mut()).await;
    job.push(0, b"new\n");
    let result = ready(waiting.as_mut()).await.unwrap();
    assert!(result.content.contains("new\n") && !result.content.contains("old\n"));
}

#[tokio::test(start_paused = true)]
async fn poll_reports_progress_only_for_changes_waits_or_input() {
    let (registry, job, _input) = fixture();
    let owner = owner();
    let output = serde_json::json!({"action": "poll", "job_id": "job-0", "return_on":"output"});
    job.push(0, b"line\n");
    assert_eq!(
        control(&registry, output.clone()).await.progress,
        Some(Progress::Advanced)
    );
    // A poll that waited for the job to finish is progress.
    let poll = serde_json::json!({"action": "poll", "job_id": "job-0"});
    let mut waiting =
        std::pin::pin!(registry.control(&owner, serde_json::from_value(poll.clone()).unwrap()));
    pending(waiting.as_mut()).await;
    tokio::time::advance(Duration::from_millis(5)).await;
    job.complete(Completion {
        status: "completed",
        exit_code: Some(0),
        error: None,
    });
    let finished = ready(waiting.as_mut()).await.unwrap();
    assert_eq!(finished.progress, Some(Progress::Advanced));
    assert_eq!(finished.idle_wait_ms, 5);
    // Re-polling the finished job returns at once with nothing new: a stall,
    // even when re-read explicitly or with a long yield_ms that poll ignores.
    for repeat in [
        poll,
        serde_json::json!({"action": "poll", "job_id": "job-0", "yield_ms": MAX_WAIT_YIELD_MS}),
        serde_json::json!({"action": "poll", "job_id": "job-0", "cursor": 0}),
    ] {
        assert_eq!(
            control(&registry, repeat).await.progress,
            Some(Progress::Stalled)
        );
    }
    let stop = serde_json::json!({"action": "stop", "job_id": "job-0"});
    assert_eq!(control(&registry, stop).await.progress, None);
}

#[tokio::test(start_paused = true)]
async fn paging_through_retained_output_is_progress() {
    let (registry, job, _input) = fixture();
    let lines: String = (0..450).map(|line| format!("line {line}\n")).collect();
    job.push(0, lines.as_bytes());
    job.complete(Completion {
        status: "completed",
        exit_code: Some(0),
        error: None,
    });
    let mut poll =
        serde_json::json!({"action": "poll", "job_id": "job-0", "yield_ms":0, "cursor":0});
    let mut pages = Vec::new();
    loop {
        let result = control(&registry, poll.clone()).await;
        let more = result.detail.as_ref().unwrap()["shell_job"]["has_more"] == true;
        pages.push(result.progress);
        poll["cursor"] = result.detail.as_ref().unwrap()["shell_job"]["cursor"].clone();
        if !more {
            break;
        }
    }
    // Every page of the finished job is new; only re-polling the end stalls.
    assert!(pages.len() > 2, "{pages:?}");
    assert!(pages.iter().all(|page| *page == Some(Progress::Advanced)));
    assert_eq!(
        control(&registry, poll).await.progress,
        Some(Progress::Stalled)
    );
}

/// Host or parent input the test releases by hand.
#[derive(Debug)]
struct TestInput(watch::Sender<bool>);

impl crate::executor::ShellInputControl for TestInput {
    fn begin_wait(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

#[tokio::test(start_paused = true)]
async fn poll_ignores_yield_ms_and_waits_past_output_until_the_job_finishes() {
    assert_eq!(AUTO_YIELD_MS, 10_000);
    for value in [
        serde_json::json!({"action":"poll","job_id":"job-0"}),
        serde_json::json!({"action":"poll","job_id":"job-0","yield_ms":1000}),
        serde_json::json!({"action":"poll","job_id":"job-0","yield_ms":0}),
    ] {
        let (registry, job, _input) = fixture();
        let owner = owner();
        let mut waiting =
            std::pin::pin!(registry.control(&owner, serde_json::from_value(value).unwrap()));
        pending(waiting.as_mut()).await;
        job.push(0, b"early output\n");
        pending(waiting.as_mut()).await;
        // Hours of quiet do not end the poll either.
        tokio::time::advance(Duration::from_secs(3 * 60 * 60)).await;
        pending(waiting.as_mut()).await;
        job.complete(Completion {
            status: "completed",
            exit_code: Some(0),
            error: None,
        });
        let result = ready(waiting.as_mut()).await.unwrap();
        assert!(result.content.contains("early output\n"));
        assert_eq!(result.detail.unwrap()["shell_job"]["status"], "completed");
        assert_eq!(result.idle_wait_ms, 3 * 60 * 60 * 1000);
    }
}

#[tokio::test(start_paused = true)]
async fn new_input_ends_a_poll_of_a_running_job_with_its_latest_output() {
    let (registry, job, _input) = fixture();
    let (input, _) = watch::channel(false);
    let input = Arc::new(TestInput(input));
    let owner = ToolExecutionContext {
        shell_input: Some(input.clone()),
        ..owner()
    };
    let args =
        serde_json::from_value(serde_json::json!({"action":"poll","job_id":"job-0"})).unwrap();
    let mut waiting = std::pin::pin!(registry.control(&owner, args));
    pending(waiting.as_mut()).await;
    job.push(0, b"early output\n");
    tokio::time::advance(Duration::from_secs(90)).await;
    job.push(
        0,
        (0..1000)
            .map(|n| format!("line {n}\n"))
            .collect::<String>()
            .as_bytes(),
    );
    pending(waiting.as_mut()).await;
    input.0.send_replace(true);
    let result = ready(waiting.as_mut()).await.unwrap();
    // The job keeps running; the poll returns the newest output and says why.
    assert!(job.running());
    assert!(result.content.contains("line 999\n"));
    assert!(result.content.contains("New input arrived"));
    assert_eq!(result.progress, Some(Progress::Advanced));
    assert_eq!(result.idle_wait_ms, 90_000);
    let detail = &result.detail.as_ref().unwrap()["shell_job"];
    assert_eq!(detail["status"], "running");
    assert_eq!(detail["cursor"], detail["output_end"]);
    assert_eq!(detail["output_gap"], true);
    let reread = control(
        &registry,
        serde_json::json!({"action":"poll","job_id":"job-0","cursor":0,"return_on":"output"}),
    )
    .await;
    assert!(reread.content.contains("early output"));
}

#[tokio::test(start_paused = true)]
async fn a_finished_job_wins_over_simultaneous_input() {
    let (registry, job, _input) = fixture();
    let (input, _) = watch::channel(false);
    let owner = ToolExecutionContext {
        shell_input: Some(Arc::new(TestInput(input.clone()))),
        ..owner()
    };
    let args =
        serde_json::from_value(serde_json::json!({"action":"poll","job_id":"job-0"})).unwrap();
    let mut waiting = std::pin::pin!(registry.control(&owner, args));
    pending(waiting.as_mut()).await;
    input.send_replace(true);
    job.complete(Completion {
        status: "completed",
        exit_code: Some(0),
        error: None,
    });
    let result = ready(waiting.as_mut()).await.unwrap();
    assert!(!result.content.contains("New input arrived"));
    assert_eq!(result.detail.unwrap()["shell_job"]["status"], "completed");
}

#[tokio::test(start_paused = true)]
async fn completion_poll_returns_on_terminal_not_new_output() {
    let (registry, job, _input) = fixture();
    let owner = owner();
    let mut waiting = std::pin::pin!(registry.control(&owner, args("poll", 0)));
    pending(waiting.as_mut()).await;
    job.push(1, b"compiler progress\n");
    pending(waiting.as_mut()).await;
    job.complete(Completion {
        status: "failed",
        exit_code: Some(7),
        error: None,
    });
    let result = ready(waiting.as_mut()).await.unwrap();
    assert!(result.is_error);
    assert_eq!(result.detail.unwrap()["shell_job"]["exit_code"], 7);
}

#[test]
fn capacity_hints_only_name_jobs_that_can_release_the_exhausted_resource() {
    fn diagnostic_job(handle: &str, terminal: bool, observed: bool, foreign: bool) -> Arc<Job> {
        let (registry, mut job, _input) = fixture();
        drop(registry);
        let mutable = Arc::get_mut(&mut job).unwrap();
        mutable.handle = handle.into();
        if foreign {
            mutable.thread = Some("foreign-thread".into());
        }
        let state = mutable.state.get_mut().unwrap();
        state.observed = observed;
        if terminal {
            state.completion = Some(Completion {
                status: "completed",
                exit_code: Some(0),
                error: None,
            });
        }
        job
    }
    let jobs: HashMap<_, _> = [
        diagnostic_job("job-1", true, true, false),
        diagnostic_job("job-2", true, false, false),
        diagnostic_job("job-3", false, false, false),
        diagnostic_job("job-4", true, false, true),
        diagnostic_job("job-5", false, false, true),
    ]
    .into_iter()
    .map(|job| (job.handle.clone(), job))
    .collect();
    let running = capacity_error(&jobs, &owner(), true).content;
    assert!(running.contains("running job-3"));
    for hidden in ["job-1", "job-2", "job-4", "job-5"] {
        assert!(!running.contains(hidden));
    }
    let retained = capacity_error(&jobs, &owner(), false).content;
    assert!(retained.contains("finished, unobserved job-2"));
    for hidden in ["job-1", "job-3", "job-4", "job-5"] {
        assert!(!retained.contains(hidden));
    }
    let other = ToolExecutionContext {
        run_id: "no-visible-jobs".into(),
        ..Default::default()
    };
    for resource in [true, false] {
        let unavailable = capacity_error(&jobs, &other, resource).content;
        assert!(unavailable.contains("No "));
        assert!(!unavailable.contains("job-"));
    }
}
