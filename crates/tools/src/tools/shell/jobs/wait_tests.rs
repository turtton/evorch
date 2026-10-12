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
        command: "read value".into(),
        owner: "owner".into(),
        call_id: None,
        thread: None,
        state: Mutex::new(State {
            output: LiveOutput::default(),
            completion: None,
            observed: false,
            completion_notified: false,
            artifact: None,
            artifact_notice: None,
            guard: None,
            process_id: None,
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
        let mut waiting = std::pin::pin!(job.wait_for_completion(yield_ms));
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
        let mut waiting = std::pin::pin!(job.wait_for_completion(60_000));
        pending(waiting.as_mut()).await;
        job.push(0, b"partial\n");
        pending(waiting.as_mut()).await;
        job.complete(Completion {
            status,
            exit_code: None,
            error: None,
        });
        ready(waiting.as_mut()).await;
        let mut already_finished = std::pin::pin!(job.wait_for_completion(60_000));
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
    let mut starting = std::pin::pin!(job.wait_for_completion(0));
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
        let mut waiting = std::pin::pin!(registry.control(&owner, args(action, 60_000)));
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
    let poll = serde_json::json!({"action": "poll", "job_id": "job-0"});
    let next = control(&registry, poll.clone()).await;
    assert!(next.content.contains("second\n"));
    assert!(!next.content.contains("first\n"));
    let reread = control(
        &registry,
        serde_json::json!({"action": "poll", "job_id": "job-0", "cursor": 0}),
    )
    .await;
    assert!(reread.content.contains("first\nsecond\n"));
    // A re-read does not move the continuation point backwards.
    let empty = control(&registry, poll).await;
    assert!(!empty.content.contains("first\n") && !empty.content.contains("second\n"));
}

#[tokio::test(start_paused = true)]
async fn omitted_cursor_poll_waits_for_output_after_the_returned_cursor() {
    let (registry, job, _input) = fixture();
    job.push(0, b"old\n");
    let _ = job.snapshot(0);
    let owner = owner();
    let mut waiting = std::pin::pin!(registry.control(&owner, args("poll", 60_000)));
    // Output already returned does not end the wait.
    pending(waiting.as_mut()).await;
    job.push(0, b"new\n");
    let result = ready(waiting.as_mut()).await.unwrap();
    assert!(result.content.contains("new\n") && !result.content.contains("old\n"));
}

#[tokio::test(start_paused = true)]
async fn poll_reports_progress_only_for_changes_or_real_waits() {
    let (registry, job, _input) = fixture();
    let owner = owner();
    let poll = |yield_ms: u64| serde_json::json!({"action": "poll", "job_id": "job-0", "yield_ms": yield_ms});
    job.push(0, b"line\n");
    assert_eq!(
        control(&registry, poll(0)).await.progress,
        Some(Progress::Advanced)
    );
    // An immediate identical result, even when re-read explicitly, is a stall.
    assert_eq!(
        control(&registry, poll(0)).await.progress,
        Some(Progress::Stalled)
    );
    let reread = serde_json::json!({"action": "poll", "job_id": "job-0", "cursor": 0});
    assert_eq!(
        control(&registry, reread).await.progress,
        Some(Progress::Stalled)
    );
    // Waiting for a quiet job is progress from the threshold on.
    for (yield_ms, expected) in [(99, Progress::Stalled), (100, Progress::Advanced)] {
        let mut waiting = std::pin::pin!(
            registry.control(&owner, serde_json::from_value(poll(yield_ms)).unwrap())
        );
        pending(waiting.as_mut()).await;
        tokio::time::advance(Duration::from_millis(yield_ms)).await;
        assert_eq!(
            ready(waiting.as_mut()).await.unwrap().progress,
            Some(expected)
        );
    }
    job.complete(Completion {
        status: "completed",
        exit_code: Some(0),
        error: None,
    });
    // A long poll of a finished job returns at once: progress once, then a stall.
    assert_eq!(
        control(&registry, poll(60_000)).await.progress,
        Some(Progress::Advanced)
    );
    assert_eq!(
        control(&registry, poll(60_000)).await.progress,
        Some(Progress::Stalled)
    );
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
    let poll = serde_json::json!({"action": "poll", "job_id": "job-0"});
    let mut pages = Vec::new();
    loop {
        let result = control(&registry, poll.clone()).await;
        let more = result.detail.as_ref().unwrap()["shell_job"]["has_more"] == true;
        pages.push(result.progress);
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
