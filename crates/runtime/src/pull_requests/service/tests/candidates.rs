//! Pull requests found in command output, and commands whose end makes the scheduler fetch.

use event_bus::{PullRequestCandidateState, ToolEvent};
use serde_json::{Value, json};
use storage::pull_requests::CandidateState;

use super::*;

fn completed(
    process: &Process,
    tool: &str,
    call: &str,
    output: &str,
    parent: Option<&str>,
    detail: Option<Value>,
) {
    process.bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: tool.into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output.into()),
        detail,
        run_id: Some("r1".into()),
        parent_call_id: parent.map(str::to_owned),
    }));
}

fn started(process: &Process, call: &str, command: &str) {
    process.bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: call.into(),
        input: Some(json!({ "command": command })),
        run_id: Some("r1".into()),
        parent_call_id: None,
    }));
}

fn job_chunk(process: &Process, job: &str, call: &str, offset: u64, chunk: &str, status: &str) {
    process.bus.emit(Event::new(ToolEvent::ShellJobOutput {
        job_uid: None,
        job_id: job.into(),
        call_id: Some(call.into()),
        run_id: Some("r1".into()),
        offset,
        chunk: chunk.into(),
        status: status.into(),
        exit_code: None,
    }));
}

fn url(number: u64) -> String {
    format!("https://github.com/{REPOSITORY}/pull/{number}\n")
}

fn presented(process: &Process) -> Vec<storage::pull_requests::PullRequestCandidate> {
    process
        .storage_handle
        .pull_request_candidates("t1", None)
        .unwrap()
}

fn candidates_changed(events: &[PullRequestEvent]) -> Vec<(u64, u64, PullRequestCandidateState)> {
    events
        .iter()
        .filter_map(|event| match event {
            PullRequestEvent::CandidateChanged {
                pull_request,
                candidate_id,
                state,
                ..
            } => Some((pull_request.number, *candidate_id, *state)),
            _ => None,
        })
        .collect()
}

fn by_number(process: &Process, number: u64) -> storage::pull_requests::PullRequestCandidate {
    presented(process)
        .into_iter()
        .find(|candidate| candidate.key == key(number))
        .unwrap()
}

fn tracked_process(world: &World) -> Process {
    let process = world.process(enabled());
    process.environment.bind("r1", "t1");
    process
}

// Given: the output of `gh pr create` / When: it is observed, twice / Then: the pull request
// is offered once, with its source, and a second sighting offers nothing new
#[tokio::test(start_paused = true)]
async fn a_pull_request_in_command_output_becomes_one_candidate() {
    let world = World::new();
    let mut process = tracked_process(&world);

    completed(
        &process,
        "shell",
        "call-1",
        &format!("created\n{}", url(7)),
        None,
        None,
    );
    completed(&process, "shell", "call-2", &url(7), None, None);
    let events = process.quiet_events().await;

    assert_eq!(
        candidates_changed(&events),
        vec![(
            7,
            presented(&process)[0].candidate_id,
            PullRequestCandidateState::Presented
        )]
    );
    let candidate = &presented(&process)[0];
    assert_eq!(candidate.key, key(7));
    assert_eq!(candidate.source_run_id, "r1");
    assert_eq!(candidate.source_call_id, "call-1");
    assert!(
        process.state("t1").active_link.is_none(),
        "nothing is linked or fetched"
    );
    assert!(process.source.calls().is_empty());
}

// Given: a nested call under run_js and a tool that only reads / When: observed / Then: the
// nested output is attributed to the visible call, and the reading tool offers nothing
#[tokio::test(start_paused = true)]
async fn nested_calls_count_and_reading_tools_do_not() {
    let world = World::new();
    let mut process = tracked_process(&world);

    completed(&process, "shell", "inner", &url(3), Some("outer"), None);
    completed(&process, "read", "read-1", &url(4), None, None);
    completed(&process, "web_fetch", "fetch-1", &url(5), None, None);
    process.quiet_events().await;

    let found = presented(&process);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, key(3));
    assert_eq!(found[0].source_call_id, "outer");
}

// Given: an asynchronous job whose URL is cut by a chunk boundary / When: its chunks arrive /
// Then: nothing is offered while it runs, and the whole URL is when it ends
#[tokio::test(start_paused = true)]
async fn a_job_is_read_across_chunks_and_offered_when_it_ends() {
    let world = World::new();
    let mut process = tracked_process(&world);
    let first = "building...\nhttps://github.com/turtton/evorch/pu";
    let second = "ll/9\ndone\n";

    job_chunk(&process, "job-1", "call-9", 0, first, "running");
    assert!(candidates_changed(&process.quiet_events().await).is_empty());
    job_chunk(
        &process,
        "job-1",
        "call-9",
        first.len() as u64,
        second,
        "completed",
    );
    let events = process.quiet_events().await;

    let found = presented(&process);
    assert_eq!(found.len(), 1, "{events:?}");
    assert_eq!(found[0].key, key(9));
    assert_eq!(found[0].source_call_id, "call-9");
}

// Given: a dismissed candidate and a confirmed one / When: the same pull requests show up again,
// also after an unlink / Then: neither comes back
#[tokio::test(start_paused = true)]
async fn decided_candidates_are_never_offered_again() {
    let world = World::new();
    let mut process = tracked_process(&world);
    let permit = process.permit("t1");
    completed(
        &process,
        "shell",
        "c1",
        &format!("{}{}", url(1), url(2)),
        None,
        None,
    );
    process.quiet_events().await;

    process
        .service
        .dismiss_candidate("t1", by_number(&process, 1).candidate_id)
        .await
        .unwrap();
    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .confirm_candidate("t1", by_number(&process, 2).candidate_id, revision, &permit)
        .await
        .unwrap();
    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .unlink("t1", revision, &permit)
        .await
        .unwrap();
    process.quiet_events().await;
    completed(
        &process,
        "shell",
        "c2",
        &format!("{}{}", url(1), url(2)),
        None,
        None,
    );
    let events = process.quiet_events().await;

    assert!(candidates_changed(&events).is_empty(), "{events:?}");
    assert_eq!(by_number(&process, 1).state, CandidateState::Dismissed);
    assert_eq!(by_number(&process, 2).state, CandidateState::Confirmed);
}

// Given: a candidate / When: it is confirmed / Then: the thread is linked to it and both
// the link and the decision are published
#[tokio::test(start_paused = true)]
async fn confirming_a_candidate_links_and_publishes_both() {
    let world = World::new();
    let mut process = tracked_process(&world);
    process.source.push_ok(&key(7), open_running(7));
    let permit = process.permit("t1");
    completed(&process, "shell", "c1", &url(7), None, None);
    process.quiet_events().await;
    let candidate = presented(&process).remove(0);

    let revision = process.state("t1").lifecycle_revision;
    process
        .service
        .confirm_candidate("t1", candidate.candidate_id, revision, &permit)
        .await
        .unwrap();
    let events = process.quiet_events().await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, PullRequestEvent::Linked { .. }))
    );
    assert!(
        candidates_changed(&events)
            .iter()
            .any(|(_, _, state)| *state == PullRequestCandidateState::Confirmed)
    );
    assert_eq!(process.state("t1").active_link.unwrap().key, key(7));
}

// Given: the pull request the thread already tracks / When: its URL shows up / Then: no chip
#[tokio::test(start_paused = true)]
async fn the_tracked_pull_request_is_not_offered() {
    let world = World::new();
    let mut process = tracked_process(&world);
    process.source.push_ok(&key(7), open_running(7));
    process.link("t1", 7).await;
    process.quiet_events().await;

    completed(&process, "shell", "c1", &url(7), None, None);
    let events = process.quiet_events().await;

    assert!(candidates_changed(&events).is_empty());
    assert!(presented(&process).is_empty());
}

// Given: tracking is off / When: command output names a pull request / Then: nothing is stored
#[tokio::test(start_paused = true)]
async fn a_disabled_service_offers_nothing() {
    let world = World::new();
    let mut process = world.process(PullRequestsConfig::default());
    process.environment.bind("r1", "t1");

    completed(&process, "shell", "c1", &url(7), None, None);
    let events = process.quiet_events().await;

    assert!(events.is_empty());
    assert!(presented(&process).is_empty());
}

// Given: a tracked thread with settled checks (the next poll is minutes away) / When: commands
// finish / Then: a push and a `gh pr` command fetch at once, an asynchronous job only when the
// job ends, and other commands do not
#[tokio::test(start_paused = true)]
async fn the_end_of_a_publishing_command_fetches_now() {
    let world = World::new();
    let process = tracked_process(&world);
    process.source.push_ok(&key(1), open_passing(1));
    process.source.repeat_last();
    process.link("t1", 1).await;
    settle().await;
    assert_eq!(process.source.calls().len(), 1);

    started(&process, "c1", "cargo test");
    completed(&process, "shell", "c1", "ok", None, None);
    settle().await;
    assert_eq!(process.source.calls().len(), 1, "an ordinary command");

    started(&process, "c2", "git push origin feat");
    settle().await;
    assert_eq!(
        process.source.calls().len(),
        1,
        "a command that has not ended"
    );
    completed(&process, "shell", "c2", "pushed", None, None);
    settle().await;
    assert_eq!(process.source.calls().len(), 2, "the push ended");

    started(&process, "c3", "gh pr create --fill");
    completed(
        &process,
        "shell",
        "c3",
        "job-1 started",
        None,
        Some(json!({ "shell_job": { "job_id": "job-1", "status": "running" } })),
    );
    settle().await;
    assert_eq!(
        process.source.calls().len(),
        2,
        "a job handle is not the end of the command"
    );
    job_chunk(&process, "job-1", "c3", 0, "working", "running");
    settle().await;
    assert_eq!(process.source.calls().len(), 2);
    job_chunk(&process, "job-1", "c3", 7, "", "completed");
    settle().await;
    assert_eq!(process.source.calls().len(), 3, "the job ended");

    started(&process, "c4", "git push");
    completed(
        &process,
        "shell",
        "c4",
        "job-2 started",
        None,
        Some(json!({ "shell_job": { "job_id": "job-2", "status": "running" } })),
    );
    completed(
        &process,
        "shell",
        "c5",
        "done",
        None,
        Some(json!({ "shell_job": { "job_id": "job-2", "status": "completed" } })),
    );
    settle().await;
    assert_eq!(
        process.source.calls().len(),
        4,
        "a poll that sees the job end counts"
    );
}
