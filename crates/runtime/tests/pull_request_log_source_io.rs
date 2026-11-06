//! ADR 0043 Phase 2 (PR B): the CI failure log source with a real process underneath.
//!
//! A stand-in `gh` (a shell script) answers `gh api` from files and `gh run view --log-failed`
//! with whatever a test makes it print, so what is checked here is what the unit tests cannot
//! see: the argument vector and environment that really reach a child, a stdin that is closed,
//! the runner's output limit (a child that is cut has no exit code of its own), a credential
//! that straddles the raw limit, and that cancelling the call ends the child. Every wait is for
//! an event (a message on a pipe, the end of a stream), never for a duration.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use runtime::pull_requests::log_source::CI_LOG_RAW_BYTES_MAX;
use runtime::pull_requests::{
    CiLogBundle, GhLogSource, LogRequest, LogSkipReason, PullRequestLogSource, render_ci_log_data,
};
use serde_json::json;
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PullRequestCheck, PullRequestKey,
};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "ghp_A1b2C3d4E5A1b2C3d4E5A1b2C3d4E5A1b2C3d4E5";

fn key() -> PullRequestKey {
    PullRequestKey::github("turtton/evorch", 481).unwrap()
}

fn check(provider_id: &str) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: provider_id.to_owned(),
        kind: CheckKind::CheckRun,
        name: "CI / build".to_owned(),
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Failure),
        head_sha: SHA.to_owned(),
    }
}

fn request(provider_id: &str) -> LogRequest {
    LogRequest::new(
        key(),
        SHA,
        Some("turtton/evorch".to_owned()),
        vec![check(provider_id)],
    )
}

fn run_json(id: u64) -> String {
    json!({
        "id": id,
        "head_sha": SHA,
        "event": "pull_request",
        "status": "completed",
        "conclusion": "failure",
        "repository": "turtton/evorch",
        "head_repository": "turtton/evorch",
        "pull_requests": [{"number": 481, "base_url": "https://api.github.com/repos/turtton/evorch"}],
    })
    .to_string()
}

/// A stand-in `gh` in its own directory.
struct FakeGh {
    dir: tempfile::TempDir,
}

impl FakeGh {
    /// `log_script` is what `gh run view --log-failed` runs (sourced by the stand-in).
    fn new(log_script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        std::fs::write(path.join("run.json"), run_json(111)).unwrap();
        std::fs::write(
            path.join("job.json"),
            json!({"id": 222, "run_id": 111, "head_sha": SHA, "status": "completed"}).to_string(),
        )
        .unwrap();
        std::fs::write(path.join("log.sh"), log_script).unwrap();
        let body = format!(
            "#!/bin/sh\n\
             dir='{dir}'\n\
             printf '%s\\n' \"$*\" >> \"$dir/calls.log\"\n\
             printf 'env %s|%s|%s\\n' \"$GH_PROMPT_DISABLED\" \"$GH_PAGER\" \"${{GH_HOST-unset}}\" >> \"$dir/env.log\"\n\
             if read -r _line; then echo stdin-not-closed >> \"$dir/env.log\"; fi\n\
             case \"$1\" in\n\
               api) case \"$6\" in\n\
                      */jobs/*) cat \"$dir/job.json\" ;;\n\
                      */runs/*) cat \"$dir/run.json\" ;;\n\
                      *) exit 3 ;;\n\
                    esac ;;\n\
               run) . \"$dir/log.sh\" ;;\n\
               *) exit 3 ;;\n\
             esac\n",
            dir = path.display()
        );
        let program = path.join("gh");
        std::fs::write(&program, body).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir }
    }

    fn source(&self) -> GhLogSource {
        GhLogSource::new().with_program(self.dir.path().join("gh").to_string_lossy())
    }

    fn lines(&self, file: &str) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join(file))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn reasons(bundle: &CiLogBundle) -> Vec<LogSkipReason> {
    bundle.skipped.iter().map(|skip| skip.reason).collect()
}

// Given: a stand-in gh / When: a job check's log is read / Then: the child got the calls in
// order with the quiet environment and a closed stdin, and the run ID came from the job
#[tokio::test]
async fn the_arguments_environment_and_order_reach_the_child() {
    let gh = FakeGh::new("printf 'build\\tTest\\terror: boom\\n'");

    let bundle = gh.source().fetch_failed_logs(&request("job:222")).await;

    let calls = gh.lines("calls.log");
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls[0].starts_with(
            "api --hostname github.com --method GET repos/turtton/evorch/actions/jobs/222 --jq "
        ),
        "{}",
        calls[0]
    );
    assert!(
        calls[1].starts_with(
            "api --hostname github.com --method GET repos/turtton/evorch/actions/runs/111 --jq "
        ),
        "{}",
        calls[1]
    );
    assert_eq!(
        calls[2],
        "run view 111 --repo github.com/turtton/evorch --log-failed"
    );
    let env = gh.lines("env.log");
    assert_eq!(
        env,
        vec!["env 1|cat|unset"; 3],
        "no prompt, no pager, no stdin: {env:?}"
    );
    assert_eq!(bundle.runs.len(), 1, "{bundle:?}");
    assert_eq!(bundle.runs[0].excerpt, "build\tTest\terror: boom");
}

// Given: a gh that is not there, one that is not signed in, and one that prints invalid UTF-8 /
// When: read / Then: each is a typed result
#[tokio::test]
async fn missing_unauthenticated_and_invalid_output_are_typed_results() {
    let missing = GhLogSource::new().with_program("/nonexistent/evorch-test-gh");
    let bundle = missing.fetch_failed_logs(&request("run:111")).await;
    assert_eq!(reasons(&bundle), vec![LogSkipReason::GhNotFound]);

    let gh = FakeGh::new("exit 0");
    std::fs::write(
        gh.dir.path().join("gh"),
        "#!/bin/sh\necho 'To get started with GitHub CLI, please run:  gh auth login' >&2\nexit 4\n",
    )
    .unwrap();
    let bundle = gh.source().fetch_failed_logs(&request("run:111")).await;
    assert_eq!(reasons(&bundle), vec![LogSkipReason::Unauthenticated]);

    let gh = FakeGh::new("printf 'bad \\377\\376 byte\\nerror\\n'");
    let bundle = gh.source().fetch_failed_logs(&request("run:111")).await;
    assert!(bundle.runs[0].lossy_utf8);
    assert_eq!(bundle.runs[0].excerpt, "bad \u{fffd}\u{fffd} byte\nerror");
}

// Given: a log of exactly the raw limit, and one a byte longer / When: read through the real
// runner / Then: the first is whole; the second ends the child (which then has no exit code)
// and is marked cut, not failed
#[tokio::test]
async fn the_raw_limit_holds_at_its_edge_with_a_real_child() {
    let line = "a".repeat(63);
    for (extra, truncated) in [(0, false), (1, true)] {
        let gh = FakeGh::new(&format!(
            "yes '{line}' | head -c {}",
            CI_LOG_RAW_BYTES_MAX + extra
        ));

        let bundle = gh.source().fetch_failed_logs(&request("run:111")).await;

        assert_eq!(bundle.runs.len(), 1, "{extra}: {:?}", bundle.skipped);
        assert_eq!(bundle.runs[0].raw_truncated, truncated, "{extra}");
        assert!(bundle.truncated);
        assert!(bundle.runs[0].excerpt.len() <= 8 * 1024);
    }
}

// Given: a child that never stops printing / When: read / Then: it is cut at the limit and the
// bundle (and its wrapper) stays within the size bounds
#[tokio::test]
async fn an_endless_log_is_cut_and_the_result_stays_bounded() {
    let gh = FakeGh::new("exec yes 'flood flood flood flood flood'");

    let bundle = gh.source().fetch_failed_logs(&request("run:111")).await;

    assert_eq!(bundle.runs.len(), 1, "{:?}", bundle.skipped);
    assert!(bundle.runs[0].raw_truncated);
    assert!(bundle.runs[0].excerpt.len() <= 8 * 1024);
    assert!(bundle.runs[0].excerpt.split('\n').count() <= 200);
    let wrapped = render_ci_log_data(&bundle, 16 * 1024);
    assert!(wrapped.len() <= 16 * 1024, "{}", wrapped.len());
}

// Given: a credential that straddles the raw limit / When: read through the real runner /
// Then: nothing of it is in the result: the cut line is dropped and the whole ones are masked
#[tokio::test]
async fn a_credential_straddling_the_raw_limit_leaves_no_piece() {
    let filler = "x".repeat(63);
    // 64-byte lines up to 128 bytes before the limit, then a line whose credential starts 100
    // bytes in, so the limit falls inside it.
    let gh = FakeGh::new(&format!(
        "yes '{filler}' | head -c {}\nprintf '%100s%s\\n' '' '{TOKEN}'\nprintf 'after the credential\\n'\n",
        CI_LOG_RAW_BYTES_MAX - 128
    ));

    let bundle = gh.source().fetch_failed_logs(&request("run:111")).await;

    let run = &bundle.runs[0];
    assert!(run.raw_truncated);
    for text in [run.excerpt.clone(), render_ci_log_data(&bundle, 16 * 1024)] {
        assert!(
            !text.contains("ghp_") && !text.contains("A1b2C3d4E5A1"),
            "{text}"
        );
    }
    assert!(!run.excerpt.contains("after the credential"));
}

fn make_fifo(path: &Path) {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

// Given: a call whose `gh run view` child is running / When: the caller cancels the call /
// Then: the child is gone (its pipe reaches end-of-file)
#[tokio::test]
async fn cancelling_the_call_ends_the_child() {
    let fifo_dir = tempfile::tempdir().unwrap();
    let fifo: PathBuf = fifo_dir.path().join("alive");
    make_fifo(&fifo);
    let gh = FakeGh::new(&format!(
        "exec 3>'{}'\necho ready >&3\nexec sleep 1000",
        fifo.display()
    ));
    let source = gh.source();
    let request = request("run:111");
    let call = tokio::spawn(async move { source.fetch_failed_logs(&request).await });

    let reader = {
        let fifo = fifo.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(fifo).unwrap();
            let mut ready = [0_u8; 6];
            file.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready\n");
            file
        })
        .await
        .unwrap()
    };

    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    let ended = tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        rest
    })
    .await
    .unwrap();
    assert!(ended.is_empty());
}
