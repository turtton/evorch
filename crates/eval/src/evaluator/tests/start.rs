use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, EventReceiver};

use super::*;
use crate::cost::Unpriced;
use crate::evaluator::groups::{GroupRecord, Member, Purpose};
use crate::evaluator::start::{Planned, estimate_value};

const CANDIDATE: &str = "[agents.worker.generation]\ntemperature = 0.2\n";

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
        unchanged.is_error && unchanged.content.contains("changes no role"),
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
    ] {
        assert!(summary.contains(expected), "{expected:?} in {summary}");
    }
    assert!(result.is_error && result.content.contains("nothing was started"));
    assert!(!fixture.user.path().join("eval").exists());
    assert_eq!(fixture.pending_notices(), 0);
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
    let expected = |thread_id: &str, problem: &str, repeat: &str, dir: &str| {
        [
            "eval",
            "run",
            "--thread",
            thread_id,
            "--candidate",
            &candidate.display().to_string(),
            "--problem",
            problem,
            "--repeat",
            repeat,
            "--output",
            dir,
            "--db",
            &fixture.recorded.config.db_path.display().to_string(),
            "--user-config",
            &fixture.user.path().display().to_string(),
        ]
        .map(str::to_owned)
        .to_vec()
    };
    assert_eq!(
        args_of(&target["dir"]),
        expected("thread-1", "skipped the failing test", "3", dir)
    );
    assert_eq!(
        args_of(&regression["dir"]),
        expected("fail-2", "slow", "1", regression["dir"].as_str().unwrap())
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
