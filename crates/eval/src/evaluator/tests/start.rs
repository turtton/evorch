use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, EventReceiver};

use super::*;
use crate::cost::Unpriced;
use crate::evaluator::start::{Planned, estimate_value};
use crate::inspection::{GROUP_SCHEMA, GroupRecord, Member, ProcessExit, Purpose};

const CANDIDATE: &str = "[agents.worker.generation]\ntemperature = 0.2\n";

#[tokio::test]
async fn restart_pause_refuses_evaluation_without_approval_or_queued_launch() {
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "regression").await;
    let mut events = fixture.bus.subscribe();
    let pause = fixture.service.producers.pause();
    let result = fixture
        .call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": CANDIDATE}),
        )
        .await;
    assert!(result.is_error && result.content.contains("busy"));
    assert!(!fixture.user.path().join("eval").exists());
    assert!(
        events
            .drain_pending_snapshot()
            .iter()
            .all(|event| !matches!(
                event.kind,
                EventKind::Tool(ToolEvent::ApprovalRequested { .. })
            ))
    );
    assert!(fixture.service.producers.resume(pause));
    fixture.service.drain_accepted().await.unwrap();
    assert!(!fixture.service.is_busy());
    assert!(!fixture.user.path().join("eval").exists());
}

/// A stand-in for `evorch eval run`: it records its arguments in the output
/// directory, then fails for `fail-*` threads, keeps running for `slow-*`
/// threads and otherwise writes a finished evaluation.
pub(super) fn fake_cli(bin: &Path) -> PathBuf {
    output::write_json(&bin.join(output::MANIFEST_FILE), &manifest(true)).unwrap();
    output::write_json(&bin.join(output::REPORT_JSON_FILE), &evaluation_report()).unwrap();
    // The Nix build sandbox has neither /bin/sh nor /usr/bin/env.
    let sh = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sh"))
        .find(|sh| sh.is_file())
        .expect("sh on PATH");
    let script = format!(
        r#"#!{sh}
prev=
for arg in "$@"; do
  case "$prev" in
    --output) out=$arg ;;
    --thread) thread=$arg ;;
  esac
  prev=$arg
done
mkdir -p "$out"
printf '%s\n' "$@" > "$out/args.txt"
case "$thread" in
  fail-*) echo "replay failed: model gpt-test is unavailable" >&2; exit 3 ;;
  slow-*) exec sleep 600 ;;
esac
cp "{bin}/{manifest}" "{bin}/{report}" "$out/"
"#,
        sh = sh.display(),
        bin = bin.display(),
        manifest = output::MANIFEST_FILE,
        report = output::REPORT_JSON_FILE,
    );
    let cli = bin.join("evorch");
    std::fs::write(&cli, script).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli
}

async fn approvals_requested(events: &mut EventReceiver) -> usize {
    let mut requested = 0;
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
        if let EventKind::Tool(ToolEvent::ApprovalRequested { .. }) = event.unwrap().kind {
            requested += 1;
        }
    }
    requested
}

async fn finished_diagnostic(events: &mut EventReceiver) -> DiagnosticEvent {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let EventKind::Diagnostic(diagnostic) = events.recv().await.unwrap().kind
                && diagnostic.code == diagnostic_codes::EVALUATION_FINISHED
            {
                return diagnostic;
            }
        }
    })
    .await
    .expect("an EvaluationFinished diagnostic")
}

fn started(result: &ToolResult) -> Value {
    assert!(!result.is_error, "eval_start failed: {}", result.content);
    serde_json::from_str(&result.content).unwrap()
}

fn args_of(dir: &Value) -> Vec<String> {
    let args = std::fs::read_to_string(Path::new(dir.as_str().unwrap()).join("args.txt")).unwrap();
    args.lines().map(str::to_owned).collect()
}

fn stop(pid: &Value) {
    let pid = rustix::process::Pid::from_raw(pid.as_i64().unwrap() as i32).unwrap();
    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
}

#[tokio::test]
async fn unpinned_threads_and_unusable_overlays_are_refused_without_asking() {
    // Given: a pinned sample and a recorded thread that is not one.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.thread("thread-2").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    let mut events = fixture.bus.subscribe();

    // When: evaluations are started for the unpinned thread, with an overlay that
    // is not TOML, one that changes the evaluation itself, and one that swaps no role.
    let start =
        |thread_id: &str, candidate: &str| json!({"thread_id": thread_id, "candidate": candidate});
    let unpinned = fixture
        .call_in_run("eval_start", start("thread-2", CANDIDATE))
        .await;
    let broken = fixture
        .call_in_run("eval_start", start("thread-1", "[agents.worker"))
        .await;
    let forbidden = fixture
        .call_in_run(
            "eval_start",
            start("thread-1", "[eval]\nrepeat_target = 9\n"),
        )
        .await;
    let unchanged = fixture
        .call_in_run(
            "eval_start",
            start("thread-1", "[sandbox]\nweb_tools_enabled = false\n"),
        )
        .await;
    let regression = fixture
        .call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": CANDIDATE, "regression_thread_ids": ["thread-2"]}),
        )
        .await;

    // Then: each is refused with its reason, nothing asked the user and nothing started.
    assert!(
        unpinned.is_error && unpinned.content.contains("not an evaluation sample"),
        "{}",
        unpinned.content
    );
    assert!(broken.is_error && broken.content.contains("unusable"));
    assert!(
        forbidden.is_error && forbidden.content.contains("`eval` cannot be changed"),
        "{}",
        forbidden.content
    );
    assert!(
        unchanged.is_error && unchanged.content.contains("changes nothing"),
        "{}",
        unchanged.content
    );
    assert!(regression.is_error && regression.content.contains("thread-2"));
    assert_eq!(approvals_requested(&mut events).await, 0);
    assert!(!fixture.user.path().join("eval").exists());
}

#[test]
fn estimates_multiply_each_original_cost_by_its_replays() {
    // Given: a target sample replayed three times and a regression sample replayed once,
    // whose original included subscription requests and an unpriced model.
    let planned = |thread_id: &str, purpose, repeat, cost| Planned {
        thread_id: thread_id.into(),
        purpose,
        problem: "slow".into(),
        repeat,
        cost,
        project_root: PathBuf::from("/project"),
        role: runtime::Role::Worker,
        sample_hash: "0123456789abcdef".into(),
    };
    let plan = [
        planned(
            "thread-1",
            Purpose::Target,
            3,
            Cost {
                unreported: Vec::new(),
                usd: 2.0,
                api_equivalent_usd: 0.0,
                unpriced: Vec::new(),
            },
        ),
        planned(
            "thread-2",
            Purpose::Regression,
            1,
            Cost {
                unreported: Vec::new(),
                usd: 0.5,
                api_equivalent_usd: 0.5,
                unpriced: vec![Unpriced {
                    provider: "local".into(),
                    profile: None,
                    model: "llama".into(),
                    requests: 2,
                }],
            },
        ),
    ];

    // When: the estimate is computed.
    let estimate = estimate_value(&plan);

    // Then: every replay of baseline and candidate costs the original again, and the
    // marks of the regression sample carry into the total.
    let target = &estimate["threads"][0];
    assert_eq!(target["replays"], 6);
    assert_eq!(target["estimate"]["usd"], 12.0);
    assert_eq!(target["estimate"]["complete"], true);
    assert_eq!(target["estimate"]["api_equivalent"], false);
    let regression = &estimate["threads"][1];
    assert_eq!(regression["purpose"], "regression");
    assert_eq!(regression["replays"], 2);
    assert_eq!(regression["original_cost"]["usd"], 0.5);
    assert_eq!(regression["estimate"]["usd"], 1.0);
    assert_eq!(regression["estimate"]["api_equivalent"], true);
    assert_eq!(regression["estimate"]["complete"], false);
    assert_eq!(
        regression["estimate"]["unpriced_models"],
        "local/llama (4 requests)"
    );
    assert_eq!(estimate["total"]["usd"], 13.0);
    assert_eq!(estimate["total"]["api_equivalent"], true);
    assert_eq!(estimate["total"]["complete"], false);
    assert!(estimate["note"].as_str().unwrap().contains("estimate"));
}

#[tokio::test]
async fn a_declined_evaluation_starts_nothing() {
    // Given: a pinned sample whose original cost $2.
    let fixture = Fixture::new();
    fixture.with_routes();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "skipped the failing test").await;

    // When: the user declines an evaluation of two repeats.
    let approval = fixture.answer_approval(false);
    let result = fixture
        .call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": CANDIDATE, "repeat": 2}),
        )
        .await;
    let shown = approval.await.unwrap();

    // Then: the user saw the sample, the replays, the estimate and the overlay,
    // and no process or group was started.
    assert_eq!(shown["kind"], "eval_start");
    let summary = shown["summary"].as_str().unwrap();
    for expected in [
        "thread-1",
        "4 replays",
        "Estimated total: $8.0000",
        "  worker: ",
        "temperature = 0.2",
        "Cost and limits:",
        "Runs: 4 replays (1 threads × baseline and candidate × repeat)",
        "2 judged pairs",
        "Models the agents run on: main/gpt-test",
        "Budget per replay, over its whole delegation tree: 5000000 tokens, 500 tool calls, 20 user turns, 3600 s",
        "up to $50.00 per replay, $200.00 for all 4 replays",
        "not a spending limit",
        "Billed separately, in neither figure above: the simulated user",
        "the judge (main/gpt-test; about 3 requests per pair, so about 6 in all",
        "Stops: a replay ends as incomplete when it reaches any budget limit",
        "pinned: sample ",
    ] {
        assert!(summary.contains(expected), "{expected:?} in {summary}");
    }
    assert!(result.is_error && result.content.contains("nothing was started"));
    assert!(!fixture.user.path().join("eval").exists());
    assert_eq!(fixture.pending_notices(), 0);
}

#[tokio::test]
async fn a_common_overlay_reaches_both_sides_and_is_shown_before_approval() {
    // Given: a pinned sample, a common overlay that fixes the worker's effort and a candidate.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    let common = "[agents.worker.generation]\nmax_tokens = 4000\n";
    let approval = fixture.answer_approval(true);

    // When: the evaluation is approved.
    let result = fixture
        .call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": CANDIDATE, "common": common, "repeat": 1}),
        )
        .await;
    let shown = approval.await.unwrap();
    let value = started(&result);

    // Then: the user saw the common overlay, and the process got it as a file next to the candidate.
    let summary = shown["summary"].as_str().unwrap();
    assert!(
        summary.contains("Common overlay (both baseline and candidate):"),
        "{summary}"
    );
    assert!(summary.contains("max_tokens = 4000"), "{summary}");
    let [target] = value["evaluations"].as_array().unwrap().as_slice() else {
        panic!("one evaluation: {value}");
    };
    let _ = fixture.notice().await;
    let args = args_of(&target["dir"]);
    let at = args.iter().position(|arg| arg == "--common").unwrap();
    assert_eq!(std::fs::read_to_string(&args[at + 1]).unwrap(), common);
}

#[tokio::test]
async fn a_common_overlay_may_set_only_models_and_roles() {
    // Given: a pinned sample.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    let mut events = fixture.bus.subscribe();

    // When: the common overlay tries to change the sandbox or the budget.
    let refused = |common: &str| {
        fixture.call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": CANDIDATE, "common": common}),
        )
    };
    let sandbox = refused("[sandbox]\nweb_tools_enabled = false\n").await;
    let eval = refused("[eval.budget]\nmax_tokens = 1\n").await;

    // Then: both are refused without asking.
    for result in [sandbox, eval] {
        assert!(
            result.is_error && result.content.contains("cannot be set by a common overlay"),
            "{}",
            result.content
        );
    }
    assert_eq!(approvals_requested(&mut events).await, 0);
}

#[tokio::test]
async fn a_tool_description_candidate_and_a_candidate_with_no_effect_are_refused_before_asking() {
    // Given: a pinned sample, and a candidate that rewrites a description, one that
    // sets a description for a tool that is not a fixed builtin, and one that only
    // restates what the baseline already has.
    let fixture = Fixture::new();
    fixture.with_routes();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    let mut events = fixture.bus.subscribe();
    let candidate =
        |tool: &str| format!("[model_tool_descriptions.main.gpt-test]\n{tool} = \"Read files.\"\n");

    // When: each is started.
    let start = |candidate: String| {
        fixture.call_in_run(
            "eval_start",
            json!({"thread_id": "thread-1", "candidate": candidate}),
        )
    };
    let description = start(candidate("read")).await;
    let run_js = start(candidate("run_js")).await;
    let restated = start("[agents.worker]\n".into()).await;

    // Then: the first waits for the judge to learn about descriptions, the others are invalid
    // comparisons, and none reached the user.
    assert!(
        description.is_error && description.content.contains("judge is not yet told"),
        "{}",
        description.content
    );
    assert!(
        run_js.is_error && run_js.content.contains("not a fixed builtin"),
        "{}",
        run_js.content
    );
    assert!(
        restated.is_error && restated.content.contains("changes nothing"),
        "{}",
        restated.content
    );
    assert_eq!(approvals_requested(&mut events).await, 0);
    assert!(!fixture.user.path().join("eval").exists());
}

#[tokio::test]
async fn a_sample_changed_while_the_approval_was_pending_starts_nothing() {
    // Given: a pinned sample, and an approval that is being answered.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    let mut events = fixture.bus.subscribe();
    let bus = Arc::clone(&fixture.bus);

    // When: the sample's problem is rewritten after the user was shown it, and then approved.
    let start = fixture.call_in_run(
        "eval_start",
        json!({"thread_id": "thread-1", "candidate": CANDIDATE, "repeat": 1}),
    );
    let answer = async {
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind {
                fixture.pin("thread-1", "a different problem").await;
                sandbox::approval::ApprovalRegistry::for_bus(&bus)
                    .resolve_presented(&call_id, true, sandbox::approval::ApprovalProvenance::CLI)
                    .expect("resolve live approval");
                return;
            }
        }
    };
    let (result, ()) = tokio::join!(start, answer);

    // Then: what was approved is not what is there, so nothing was started.
    assert!(
        result.is_error
            && result
                .content
                .contains("changed while the approval was pending"),
        "{}",
        result.content
    );
    assert!(!fixture.user.path().join("eval").exists());
}

#[tokio::test]
async fn an_approved_evaluation_runs_every_sample_and_reports_once() {
    // Given: a pinned target sample and a pinned regression sample whose replay fails.
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    fixture.thread("fail-2").await;
    fixture.pin("thread-1", "skipped the failing test").await;
    fixture.pin("fail-2", "slow").await;
    let mut events = fixture.bus.subscribe();

    // When: the user approves the evaluation.
    let approval = fixture.answer_approval(true);
    let result = fixture
        .call_in_run(
            "eval_start",
            json!({
                "thread_id": "thread-1",
                "candidate": CANDIDATE,
                "regression_thread_ids": ["fail-2"],
                "repeat": 3,
            }),
        )
        .await;
    approval.await.unwrap();
    let value = started(&result);
    let (run_id, notice) = fixture.notice().await;
    let diagnostic = finished_diagnostic(&mut events).await;

    // Then: one `eval run` per sample started with its own output directory under
    // eval/, the candidate file, the database and the user configuration.
    let group = value["group"].as_str().unwrap();
    let [target, regression] = value["evaluations"].as_array().unwrap().as_slice() else {
        panic!("two evaluations: {value}");
    };
    assert_eq!(target["purpose"], "target");
    assert_eq!(regression["purpose"], "regression");
    let eval = fixture.user.path().join("eval");
    let candidate = eval.join(".groups").join(group).join("candidate.toml");
    assert_eq!(std::fs::read_to_string(&candidate).unwrap(), CANDIDATE);
    let dir = target["dir"].as_str().unwrap();
    assert!(Path::new(dir).starts_with(&eval), "{dir}");
    let expected = |thread_id: &str, problem: &str, repeat: &str, dir: &str, digest: &str| {
        [
            "eval",
            "run",
            "--thread",
            thread_id,
            "--candidate",
            &candidate.display().to_string(),
            "--approved-digest",
            digest,
            "--problem",
            problem,
            "--repeat",
            repeat,
            "--output",
            dir,
            "--user-config",
            &fixture.user.path().display().to_string(),
        ]
        .map(str::to_owned)
        .to_vec()
    };
    let digest_of = |value: &Value| {
        let args = args_of(value);
        let at = args
            .iter()
            .position(|arg| arg == "--approved-digest")
            .unwrap();
        args[at + 1].clone()
    };
    let (target_digest, regression_digest) =
        (digest_of(&target["dir"]), digest_of(&regression["dir"]));
    for digest in [&target_digest, &regression_digest] {
        assert!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "{digest}"
        );
    }
    assert_ne!(
        target_digest, regression_digest,
        "each sample is approved on its own inputs"
    );
    assert_eq!(
        args_of(&target["dir"]),
        expected(
            "thread-1",
            "skipped the failing test",
            "3",
            dir,
            &target_digest
        )
    );
    assert_eq!(
        args_of(&regression["dir"]),
        expected(
            "fail-2",
            "slow",
            "1",
            regression["dir"].as_str().unwrap(),
            &regression_digest
        )
    );

    // Then: exactly one notice reached the starting run with each evaluation's outcome,
    // the failed one with its exit status and the end of its stderr.
    assert_eq!(run_id, "run-7");
    assert!(notice.contains(&format!("Evaluation group {group} finished: 1 of 2")));
    assert!(notice.contains(&format!("- target thread-1: report in {dir}; ")));
    assert!(notice.contains(
        "- regression fail-2: failed (exit code 3); stderr ends with: replay failed: model gpt-test is unavailable"
    ));
    assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
    assert_eq!(diagnostic.run_id.as_deref(), Some("run-7-root"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fixture.pending_notices(), 0);

    // Then: the group's report shows both outcomes.
    let detail = fixture.json("eval_report", json!({"group": group})).await;
    assert_eq!(detail["evaluations"][0]["state"], "finished");
    assert!(
        detail["evaluations"][0]["report"]
            .as_str()
            .unwrap()
            .contains("cost baseline")
    );
    assert_eq!(detail["evaluations"][1]["state"], "failed");
    assert_eq!(detail["evaluations"][1]["failure"], "exit code 3");
    assert!(
        detail["evaluations"][1]["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("unavailable")
    );
    assert_eq!(detail["candidate"], CANDIDATE);
}

#[tokio::test]
async fn running_groups_show_progress_and_unfinished_ones_from_earlier_sessions_are_interrupted() {
    // Given: a group whose replay is still running, and a group an earlier GUI
    // session left unfinished.
    let fixture = Fixture::new();
    fixture.thread("slow-1").await;
    fixture.pin("slow-1", "slow").await;
    let approval = fixture.answer_approval(true);
    let value = started(
        &fixture
            .call_in_run(
                "eval_start",
                json!({"thread_id": "slow-1", "candidate": CANDIDATE, "repeat": 1}),
            )
            .await,
    );
    approval.await.unwrap();
    let group = value["group"].as_str().unwrap();
    let running = PathBuf::from(value["evaluations"][0]["dir"].as_str().unwrap());
    std::fs::create_dir_all(&running).unwrap();
    output::write_json(&running.join(output::MANIFEST_FILE), &manifest(false)).unwrap();
    let eval = fixture.user.path().join("eval");
    let stale = eval.join("20200101-000000-thread-1");
    std::fs::create_dir_all(&stale).unwrap();
    output::write_json(&stale.join(output::MANIFEST_FILE), &manifest(false)).unwrap();
    let earlier = eval.join(".groups").join("20200101-000000-abcdef");
    std::fs::create_dir_all(&earlier).unwrap();
    let record = GroupRecord {
        schema: GROUP_SCHEMA,
        id: "20200101-000000-abcdef".into(),
        run_id: "run-1".into(),
        started_at: "2020-01-01T00:00:00.000Z".into(),
        candidate: earlier.join("candidate.toml"),
        members: vec![Member {
            thread_id: "thread-1".into(),
            purpose: Purpose::Target,
            repeat: 1,
            dir: stale.clone(),
            pid: Some(1),
            stdout: earlier.join("0.stdout.log"),
            stderr: earlier.join("0.stderr.log"),
            exit: None,
        }],
        finished_at: None,
        notice: None,
    };
    output::write_json(&earlier.join("group.json"), &record).unwrap();

    // When: the running group and the whole listing are read.
    let detail = fixture.json("eval_report", json!({"group": group})).await;
    let listed = fixture.json("eval_report", json!({})).await;
    stop(&value["evaluations"][0]["pid"]);

    // Then: the running group shows its progress; the earlier one is interrupted.
    assert_eq!(detail["finished_at"], Value::Null);
    assert_eq!(detail["evaluations"][0]["state"], "running");
    assert_eq!(detail["evaluations"][0]["progress"]["planned_runs"], 2);
    assert_eq!(detail["evaluations"][0]["progress"]["finished_runs"], 0);
    let state = |name: &str| {
        listed["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|evaluation| evaluation["dir"] == name)
            .map(|evaluation| evaluation["state"].clone())
    };
    let running_name = running.file_name().unwrap().to_str().unwrap();
    assert_eq!(state(running_name), Some(json!("running")));
    assert_eq!(
        state("20200101-000000-thread-1"),
        Some(json!("interrupted"))
    );
    let groups = listed["groups"].as_array().unwrap();
    assert_eq!(groups[0]["group"], group);
    assert_eq!(groups[0]["evaluations"][0]["state"], "running");
    assert_eq!(groups[1]["evaluations"][0]["state"], "interrupted");
    assert!(
        fixture
            .call("eval_report", json!({"group": "../escape"}))
            .await
            .is_error
    );

    // Then: once the process ends the group reports it.
    let (_, notice) = fixture.notice().await;
    assert!(notice.contains("killed by signal 9"), "{notice}");
}

#[test]
fn the_cli_is_found_beside_the_gui_or_else_on_the_path() {
    // Given: a GUI with an executable CLI beside it, one with a non-executable file
    // there, and a PATH directory holding the CLI.
    let executable = |dir: &Path, mode| {
        std::fs::create_dir_all(dir).unwrap();
        let cli = dir.join("evorch");
        std::fs::write(&cli, "").unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(mode)).unwrap();
        cli
    };
    let root = tempfile::tempdir().unwrap();
    let beside = executable(&root.path().join("bundle"), 0o755);
    let plain = executable(&root.path().join("plain"), 0o644);
    let on_path = executable(&root.path().join("path"), 0o755);
    let path = std::env::join_paths([root.path().join("empty"), root.path().join("path")]).unwrap();

    // When / Then: the sibling wins, a non-executable sibling falls back to PATH,
    // and with neither the error names where the CLI was expected.
    let gui = |dir: &str| root.path().join(dir).join("evorch-gui");
    assert_eq!(find_cli(Some(&gui("bundle")), Some(&path)), Ok(beside));
    assert_eq!(find_cli(Some(&gui("plain")), Some(&path)), Ok(on_path));
    let missing = find_cli(Some(&gui("plain")), None).unwrap_err();
    assert!(missing.contains(&plain.display().to_string()), "{missing}");
}

#[tokio::test]
async fn groups_from_other_processes_are_judged_by_their_files_not_assumed_running() {
    // Given: three groups no process of this session launched: one with a finished
    // manifest and report but no recorded exit, one unfinished, and one whose
    // process was killed before it reported.
    let fixture = Fixture::new();
    let eval = fixture.user.path().join("eval");
    let group = |id: &str, dir: &str, exit: Option<ProcessExit>, report: bool, finished: bool| {
        let dir = eval.join(dir);
        let home = eval.join(".groups").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        output::write_json(&dir.join(output::MANIFEST_FILE), &manifest(finished)).unwrap();
        if report {
            output::write_json(&dir.join(output::REPORT_JSON_FILE), &evaluation_report()).unwrap();
        }
        std::fs::write(home.join("0.stderr.log"), "boom\n").unwrap();
        let record = GroupRecord {
            schema: GROUP_SCHEMA,
            id: id.into(),
            run_id: "run-1".into(),
            started_at: "2020-01-01T00:00:00.000Z".into(),
            candidate: home.join("candidate.toml"),
            members: vec![Member {
                thread_id: "thread-1".into(),
                purpose: Purpose::Target,
                repeat: 1,
                dir,
                pid: Some(1),
                stdout: home.join("0.stdout.log"),
                stderr: home.join("0.stderr.log"),
                exit,
            }],
            finished_at: None,
            notice: None,
        };
        output::write_json(&home.join("group.json"), &record).unwrap();
    };
    group("20200101-000000-aaaaaa", "done", None, true, true);
    group("20200102-000000-bbbbbb", "unfinished", None, false, false);
    let killed = ProcessExit {
        code: None,
        signal: Some(9),
        error: None,
        finished_at: "2020-01-02T00:00:00.000Z".into(),
    };
    group(
        "20200103-000000-cccccc",
        "killed",
        Some(killed),
        false,
        false,
    );

    // When: each group is read.
    let read = |id: &'static str| fixture.json("eval_report", json!({"group": id}));
    let done = read("20200101-000000-aaaaaa").await;
    let unfinished = read("20200102-000000-bbbbbb").await;
    let killed = read("20200103-000000-cccccc").await;

    // Then: a finished report is finished, an unconfirmed process is not running, and a
    // killed one failed with the end of its stderr.
    assert_eq!(done["evaluations"][0]["state"], "finished");
    assert_eq!(done["evaluations"][0]["phase"], "finished");
    assert!(done["evaluations"][0]["report"].is_string());
    assert_eq!(unfinished["evaluations"][0]["state"], "interrupted");
    assert_eq!(unfinished["evaluations"][0]["phase"], "unverified");
    assert_eq!(killed["evaluations"][0]["state"], "failed");
    assert_eq!(killed["evaluations"][0]["failure"], "killed by signal 9");
    assert_eq!(killed["evaluations"][0]["stderr_tail"], "boom");
    assert_eq!(killed["evaluations"][0]["progress"]["finished_runs"], 0);
}

// Given: an evaluation launch parked on its blocking thread / When: the caller
// is dropped and a restart waits for the producers to drain / Then: the launch's
// producer permit is still held, so the restart cannot see the producers drained
// before the launch registers its evaluations, and it is released only once the
// launch returns.
#[tokio::test]
async fn an_abandoned_launch_keeps_its_producer_permit_until_it_returns() {
    let gate = runtime::producer::ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let caller = tokio::spawn(crate::evaluator::start::launch_holding(permit, move || {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Ok(7)
    }));
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();

    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(gate.active(), 1, "the permit left with the caller");
    let mut drained = std::pin::pin!(gate.drained());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(
            std::future::Future::poll(drained.as_mut(), cx).is_pending()
        ))
        .await,
        "the producers drained while a launch was still running"
    );

    release_tx.send(()).unwrap();
    drained.await;
    assert_eq!(gate.active(), 0);
}
