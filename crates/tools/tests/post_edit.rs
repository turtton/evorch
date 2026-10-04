use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use config::CommentCheckerConfig;
use event_bus::EventBus;
use sandbox::{CommandSpec, DirectSandbox, Sandbox, SandboxError, WrappedCommand};
use serde_json::json;
use tools::post_edit::{
    CommentChecker, DEFAULT_PROMPT, PostEditHook, PostEditInput, PostEditOutcome, UnavailableReason,
};
use tools::{Edit, Tool, ToolExecutionContext, ToolExecutor, Write};
use tracing::instrument::WithSubscriber;

#[derive(Default)]
struct RecordingSandbox(Mutex<Vec<CommandSpec>>);
impl Sandbox for RecordingSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        self.0.lock().unwrap().push(spec.clone());
        DirectSandbox::new_unchecked().wrap(spec)
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    binary: PathBuf,
    sandbox: Arc<RecordingSandbox>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("fixture checker");
        let fixture = env!("CARGO_BIN_EXE_comment-checker-test-fixture").replace('\'', "'\\''");
        std::fs::write(
            &binary,
            include_str!("support/comment_checker.sh")
                .replace("__COMMENT_CHECKER_TEST_FIXTURE__", &fixture),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            binary,
            sandbox: Arc::new(RecordingSandbox::default()),
        }
    }
    fn config(&self, prompt: Option<&str>) -> CommentCheckerConfig {
        CommentCheckerConfig {
            binary: self.binary.to_str().unwrap().into(),
            prompt: prompt.map(str::to_owned),
            ..Default::default()
        }
    }
    fn checker(&self, prompt: Option<&str>) -> Arc<CommentChecker> {
        Arc::new(CommentChecker::resolve(self.sandbox.clone(), &self.config(prompt)).unwrap())
    }
    fn file(&self) -> PathBuf {
        self.dir.path().join("source.rs")
    }
    fn input(&self) -> PostEditInput {
        PostEditInput {
            tool_name: "Write",
            file_path: self.file(),
            content: Some("fn main() {}".into()),
            old_string: None,
            new_string: None,
        }
    }
    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(path)).unwrap()
    }
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
impl Logs {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(self.clone())
            .finish()
    }
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

// T1: fixed sandbox command, exact JSON, default prompt, successful output unchanged.
#[tokio::test]
async fn pass_preserves_write_and_edit_results_and_sends_only_present_input_fields() {
    let f = Fixture::new();
    let checker = f.checker(None);
    assert_eq!(checker.check(&f.input()).await, PostEditOutcome::Pass);
    let args = json!({"path": f.file(), "content": "old 日本語"});
    let baseline = Write.execute(args.clone()).await.unwrap();
    std::fs::remove_file(f.file()).unwrap();
    let result = Write
        .with_post_edit_hook(checker.clone())
        .execute(args)
        .await
        .unwrap();
    assert_eq!(result, baseline);
    let payload: serde_json::Value = serde_json::from_str(&f.read("checker.input")).unwrap();
    assert_eq!(
        payload,
        json!({"tool_name": "Write", "tool_input": {"file_path": f.file(), "content": "old 日本語"}})
    );
    let args = json!({"path": f.file(), "old_string": "old", "new_string": "new"});
    let baseline = Edit.execute(args.clone()).await.unwrap();
    std::fs::write(f.file(), "old 日本語").unwrap();
    let result = Edit
        .with_post_edit_hook(checker)
        .execute(args)
        .await
        .unwrap();
    assert_eq!(result, baseline);
    let payload: serde_json::Value = serde_json::from_str(&f.read("checker.input")).unwrap();
    assert_eq!(
        payload,
        json!({"tool_name": "Edit", "tool_input": {"file_path": f.file(), "old_string": "old", "new_string": "new"}})
    );
    assert_eq!(
        f.read("checker.args"),
        format!("check\n--prompt\n{DEFAULT_PROMPT}\n")
    );
    assert_eq!(Path::new(f.read("checker.cwd").trim()), f.dir.path());
    for spec in f.sandbox.0.lock().unwrap().iter() {
        assert_eq!(Path::new(&spec.program), f.binary.canonicalize().unwrap());
        assert_eq!(spec.args, ["check", "--prompt", DEFAULT_PROMPT]);
        assert_eq!(spec.cwd.as_deref(), Some(f.dir.path()));
        assert!(spec.extra_env.is_empty());
    }
}

// T2: warning is advisory for both tools; checker stdin and prompt can select it.
#[tokio::test]
async fn warning_appends_text_and_detail_without_failing_the_edit() {
    let f = Fixture::new();
    let checker = f.checker(Some("warning"));
    assert_eq!(
        checker.check(&f.input()).await,
        PostEditOutcome::Warning {
            message: "explain why, not what".into()
        }
    );
    for edit in [false, true] {
        std::fs::write(f.file(), "old").unwrap();
        let (tool, baseline): (Box<dyn Tool>, _) = if edit {
            (
                Box::new(Edit.with_post_edit_hook(checker.clone())),
                Box::new(Edit) as Box<dyn Tool>,
            )
        } else {
            (
                Box::new(Write.with_post_edit_hook(checker.clone())),
                Box::new(Write) as Box<dyn Tool>,
            )
        };
        let args = if edit {
            json!({"path": f.file(), "old_string": "old", "new_string": "new"})
        } else {
            json!({"path": f.file(), "content": "new"})
        };
        let expected = baseline.execute(args.clone()).await.unwrap();
        std::fs::write(f.file(), "old").unwrap();
        let result = tool.execute(args).await.unwrap();
        assert_eq!(
            result.content,
            format!(
                "{}\n[comment-checker warning]\nExternal comment-checker diagnostic (untrusted; quoted):\n> explain why, not what\n[end comment-checker warning]",
                expected.content
            )
        );
        assert!(!result.is_error);
        assert_eq!(
            result.detail,
            Some(
                json!({"comment_checker": {"outcome": "warning", "message": "explain why, not what"}})
            )
        );
        assert_eq!(std::fs::read_to_string(f.file()).unwrap(), "new");
    }
    let input = PostEditInput {
        content: Some("fixture-warning".into()),
        ..f.input()
    };
    assert!(matches!(
        f.checker(None).check(&input).await,
        PostEditOutcome::Warning { .. }
    ));
}

// T3a/T3b: automatic absence is silent; an explicit unusable path warns once at resolution.
#[test]
fn absent_auto_discovery_is_silent_but_explicit_missing_or_non_executable_path_warns() {
    let f = Fixture::new();
    let logs = Logs::default();
    tracing::subscriber::with_default(logs.subscriber(), || {
        let config = CommentCheckerConfig {
            binary: format!(
                "evorch-no-checker-{}",
                f.dir.path().file_name().unwrap().to_str().unwrap()
            ),
            ..Default::default()
        };
        assert!(CommentChecker::resolve(f.sandbox.clone(), &config).is_none());
        assert!(logs.text().is_empty());
        let mut config = f.config(None);
        config.binary = f.dir.path().join("missing").to_str().unwrap().into();
        assert!(CommentChecker::resolve(f.sandbox.clone(), &config).is_none());
        assert_eq!(logs.text().matches("WARN").count(), 1);
        std::fs::set_permissions(&f.binary, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(CommentChecker::resolve(f.sandbox.clone(), &f.config(None)).is_none());
        assert_eq!(logs.text().matches("WARN").count(), 2);
    });
    assert!(f.sandbox.0.lock().unwrap().is_empty());
    assert!(!f.dir.path().join("checker.calls").exists());
}

// T4: outcomes stay truthful while shared Write/Edit availability diagnostics are one-shot.
#[tokio::test]
async fn abnormal_exit_notifies_once_across_write_and_edit() {
    let f = Fixture::new();
    let checker = f.checker(Some("abnormal"));
    for _ in 0..2 {
        assert!(matches!(
            checker.check(&f.input()).await,
            PostEditOutcome::Unavailable {
                reason: UnavailableReason::UnexpectedExitCode(1),
                ..
            }
        ));
    }
    let logs = Logs::default();
    async {
        let result = Write
            .with_post_edit_hook(checker.clone())
            .execute(json!({"path": f.file(), "content": "old"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(!result.content.contains("comment-checker"));
        let detail = result.detail.unwrap();
        assert_eq!(detail["comment_checker"]["outcome"], "unavailable");
        assert_eq!(
            detail["comment_checker"]["reason"],
            json!({"unexpected_exit_code": 1})
        );
        assert_eq!(
            logs.text().matches("comment-checker unavailable").count(),
            1
        );
        let result = Edit
            .with_post_edit_hook(checker.clone())
            .execute(json!({"path": f.file(), "old_string": "old", "new_string": "new"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.detail.is_none());
        assert!(!result.content.contains("comment-checker"));
        assert_eq!(
            logs.text().matches("comment-checker unavailable").count(),
            1
        );
    }
    .with_subscriber(logs.subscriber())
    .await;
    assert_eq!(f.read("checker.calls").lines().count(), 4);
}

// Keep the paused clock from auto-advancing while the real external fixture
// reaches its IPC handshake. Only explicit advance() can expire its deadline.
struct FrozenClock(tokio::task::JoinHandle<()>);
impl FrozenClock {
    fn new() -> Self {
        Self(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    }
}
impl Drop for FrozenClock {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Handshakes {
    leader: tokio::net::UnixListener,
    descendant: tokio::net::UnixListener,
}
impl Handshakes {
    fn new(f: &Fixture) -> Self {
        Self {
            leader: tokio::net::UnixListener::bind(f.dir.path().join("checker.ready.sock"))
                .unwrap(),
            descendant: tokio::net::UnixListener::bind(f.dir.path().join("checker.child.sock"))
                .unwrap(),
        }
    }
    async fn accept(
        listener: &tokio::net::UnixListener,
    ) -> (
        tokio::io::BufReader<tokio::net::UnixStream>,
        rustix::process::Pid,
    ) {
        use tokio::io::AsyncBufReadExt;
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = tokio::io::BufReader::new(stream);
        let mut pid = String::new();
        stream.read_line(&mut pid).await.unwrap();
        (
            stream,
            rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap(),
        )
    }
}

async fn assert_closed(mut stream: tokio::io::BufReader<tokio::net::UnixStream>) {
    use tokio::io::AsyncReadExt;
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
}

#[tokio::test(start_paused = true)]
async fn timeout_kills_group_and_reaps_checker_even_with_unread_input() {
    let _clock = FrozenClock::new();
    for mode in ["timeout", "unread-input"] {
        let f = Fixture::new();
        let handshakes = Handshakes::new(&f);
        let checker = f.checker(Some(mode));
        let mut input = f.input();
        if mode == "unread-input" {
            input.content = Some("x".repeat(1024 * 1024));
        }
        let check = checker.clone();
        let task = tokio::spawn(async move { check.check(&input).await });
        let (leader, pid) = Handshakes::accept(&handshakes.leader).await;
        let (descendant, _) = Handshakes::accept(&handshakes.descendant).await;
        assert!(rustix::process::test_kill_process(pid).is_ok());
        tokio::time::advance(Duration::from_millis(15_001)).await;
        assert!(matches!(
            task.await.unwrap(),
            PostEditOutcome::Unavailable {
                reason: UnavailableReason::Timeout,
                ..
            }
        ));
        checker.drain().await;
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
        assert_closed(leader).await;
        assert_closed(descendant).await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_notifies_supervisor_and_drain_waits_for_group_cleanup() {
    let _clock = FrozenClock::new();
    let f = Fixture::new();
    let handshakes = Handshakes::new(&f);
    let checker = f.checker(Some("unread-input"));
    let check = checker.clone();
    let input = PostEditInput {
        content: Some("x".repeat(1024 * 1024)),
        ..f.input()
    };
    let task = tokio::spawn(async move { check.check(&input).await });
    let (leader, pid) = Handshakes::accept(&handshakes.leader).await;
    let (descendant, _) = Handshakes::accept(&handshakes.descendant).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // No clock advance: cancellation, not the deadline, must initiate cleanup.
    checker.drain().await;
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert_closed(leader).await;
    assert_closed(descendant).await;
}

#[tokio::test(start_paused = true)]
async fn leader_exit_cleans_descendants_holding_output_pipes_without_waiting_for_timeout() {
    use tokio::io::AsyncWriteExt;
    let _clock = FrozenClock::new();
    let f = Fixture::new();
    let handshakes = Handshakes::new(&f);
    let checker = f.checker(Some("descendant-pipe"));
    let check = checker.clone();
    let input = f.input();
    let task = tokio::spawn(async move { check.check(&input).await });
    let (mut leader, pid) = Handshakes::accept(&handshakes.leader).await;
    let (descendant, _) = Handshakes::accept(&handshakes.descendant).await;
    leader.get_mut().write_all(b"x").await.unwrap();
    assert_eq!(task.await.unwrap(), PostEditOutcome::Pass);
    checker.drain().await;
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert_closed(descendant).await;
}

// T6: every failed editing path bypasses the subprocess entirely.
#[tokio::test]
async fn unsuccessful_edits_never_invoke_the_hook() {
    let f = Fixture::new();
    let checker = f.checker(Some("warning"));
    let write = Write.with_post_edit_hook(checker.clone());
    assert!(
        write
            .execute(json!({"path": f.dir.path().join("missing/file"), "content": "x"}))
            .await
            .is_err()
    );
    assert!(write.execute(json!({"path": f.file()})).await.is_err());
    let edit = Edit.with_post_edit_hook(checker);
    assert!(
        edit.execute(json!({"path": f.file(), "old_string": "old", "new_string": "new"}))
            .await
            .is_err()
    );
    std::fs::write(f.file(), "old").unwrap();
    for old in ["", "not found"] {
        assert!(
            edit.execute(json!({"path": f.file(), "old_string": old, "new_string": "new"}))
                .await
                .is_err()
        );
    }
    assert_eq!(std::fs::read_to_string(f.file()).unwrap(), "old");
    assert!(f.sandbox.0.lock().unwrap().is_empty());
    assert!(!f.dir.path().join("checker.calls").exists());
}

#[tokio::test]
async fn resolved_binary_removed_before_launch_reports_spawn_failed() {
    let f = Fixture::new();
    let checker = f.checker(None);
    std::fs::remove_file(&f.binary).unwrap();
    assert!(matches!(
        checker.check(&f.input()).await,
        PostEditOutcome::Unavailable {
            reason: UnavailableReason::SpawnFailed,
            ..
        }
    ));
}

#[tokio::test]
async fn sandbox_wrap_failure_is_io_and_never_falls_back_to_direct_execution() {
    struct Reject;
    impl Sandbox for Reject {
        fn wrap(&self, _: CommandSpec) -> Result<WrappedCommand, SandboxError> {
            Err(SandboxError::InvalidSpec {
                detail: "fixture rejection".into(),
            })
        }
    }
    let f = Fixture::new();
    let checker = CommentChecker::resolve(Arc::new(Reject), &f.config(None)).unwrap();
    assert!(matches!(
        checker.check(&f.input()).await,
        PostEditOutcome::Unavailable {
            reason: UnavailableReason::Io,
            ..
        }
    ));
    assert!(!f.dir.path().join("checker.calls").exists());
}

// T9: disabled checks neither resolve nor launch; default tool capabilities remain unchanged.
#[tokio::test]
async fn disabled_checker_preserves_executor_behavior_and_permissions() {
    let f = Fixture::new();
    let logs = Logs::default();
    let mut config = f.config(Some("warning"));
    config.enabled = false;
    let hook = tracing::subscriber::with_default(logs.subscriber(), || {
        assert!(
            CommentChecker::resolve(
                f.sandbox.clone(),
                &CommentCheckerConfig {
                    binary: "/not/a/checker".into(),
                    ..config.clone()
                }
            )
            .is_none()
        );
        CommentChecker::resolve(f.sandbox.clone(), &config)
            .map(|c| Arc::new(c) as Arc<dyn PostEditHook>)
    });
    assert!(hook.is_none());
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        f.sandbox.clone(),
        Some(f.dir.path().to_path_buf()),
    )
    .with_post_edit_hook(hook);
    let args = json!({"path": f.file(), "content": "fn main() {}"});
    let baseline = Write.execute(args.clone()).await.unwrap();
    std::fs::remove_file(f.file()).unwrap();
    let result = executor
        .execute(&ctx(), "write", "call-1", args)
        .await
        .unwrap();
    assert_eq!(result.content, baseline.content);
    assert_eq!(result.detail, baseline.detail);
    assert!(!result.is_error);
    assert!(logs.text().is_empty());
    assert!(f.sandbox.0.lock().unwrap().is_empty());
    assert!(!f.dir.path().join("checker.calls").exists());
    for name in ["write", "edit"] {
        let capabilities = executor.tool_permissions(name).unwrap();
        assert!(!capabilities.process_spawn);
        assert!(capabilities.fs_write);
    }
}

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: "run-1".into(),
        thread_id: None,
        call_id: None,
    }
}

#[tokio::test]
async fn executor_injects_shared_hook_without_changing_schemas_or_permission_flags() {
    let f = Fixture::new();
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::new(EventBus::new(16)),
        f.sandbox.clone(),
        Some(f.dir.path().to_path_buf()),
    )
    .with_post_edit_hook(Some(f.checker(Some("warning"))));
    for (name, args) in [
        ("write", json!({"path": "source.rs", "content": "old"})),
        (
            "edit",
            json!({"path": "source.rs", "old_string": "old", "new_string": "new"}),
        ),
    ] {
        let result = executor.execute(&ctx(), name, "call", args).await.unwrap();
        assert!(
            result
                .content
                .contains("> explain why, not what\n[end comment-checker warning]")
        );
        assert!(!result.is_error);
        assert_eq!(
            result.detail.unwrap()["comment_checker"]["outcome"],
            "warning"
        );
        assert!(!executor.tool_permissions(name).unwrap().process_spawn);
    }
    assert_eq!(f.read("checker.calls").lines().count(), 2);
    let executor = executor.with_post_edit_hook(None);
    let result = executor
        .execute(
            &ctx(),
            "write",
            "call-3",
            json!({"path": "source.rs", "content": "final"}),
        )
        .await
        .unwrap();
    assert!(result.detail.is_none());
    assert_eq!(f.read("checker.calls").lines().count(), 2);
}

#[tokio::test]
async fn existing_write_uses_old_full_text_and_noops_never_start_checker() {
    let f = Fixture::new();
    let checker = f.checker(Some("comments"));
    let write = Write.with_post_edit_hook(checker.clone());
    let before = "// old comment\nfn old() {}\n";
    std::fs::write(f.file(), before).unwrap();
    let result = write
        .execute(json!({"path": f.file(), "content": before}))
        .await
        .unwrap();
    assert!(result.detail.is_none());
    assert!(f.sandbox.0.lock().unwrap().is_empty());
    let result = Edit
        .with_post_edit_hook(checker)
        .execute(json!({"path": f.file(), "old_string": "old", "new_string": "old"}))
        .await
        .unwrap();
    assert!(result.detail.is_none());
    assert!(f.sandbox.0.lock().unwrap().is_empty());
    let after = "// old   comment\nfn new() {}\n";
    let result = write
        .execute(json!({"path": f.file(), "content": after}))
        .await
        .unwrap();
    assert!(
        result.detail.is_none(),
        "normalized old comments must not warn"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.read("checker.input")).unwrap(),
        json!({"tool_name": "Edit", "tool_input": {"file_path": f.file(), "old_string": before, "new_string": after}})
    );
    let result = write
        .execute(json!({"path": f.file(), "content": "// new comment\nfn new() {}\n"}))
        .await
        .unwrap();
    assert_eq!(
        result.detail.unwrap()["comment_checker"]["outcome"],
        "warning"
    );
    assert!(!result.is_error);
}

#[tokio::test]
async fn new_write_and_edit_check_only_new_or_changed_comments() {
    let f = Fixture::new();
    let checker = f.checker(Some("comments"));
    let result = Write
        .with_post_edit_hook(checker.clone())
        .execute(json!({"path": f.file(), "content": "// existing comment\nfn old() {}"}))
        .await
        .unwrap();
    assert_eq!(
        result.detail.unwrap()["comment_checker"]["outcome"],
        "warning"
    );
    let edit = Edit.with_post_edit_hook(checker);
    let result = edit
        .execute(json!({"path": f.file(), "old_string": "fn old()", "new_string": "fn new()"}))
        .await
        .unwrap();
    assert!(result.detail.is_none());
    let result = edit.execute(json!({"path": f.file(), "old_string": "// existing comment", "new_string": "// changed comment"})).await.unwrap();
    assert_eq!(
        result.detail.unwrap()["comment_checker"]["outcome"],
        "warning"
    );
}

#[tokio::test]
async fn unreadable_previous_text_skips_check_but_keeps_successful_write() {
    let f = Fixture::new();
    // Invalid UTF-8 is a deterministic read failure, including when tests run as root.
    std::fs::write(f.file(), [0xff]).unwrap();
    let result = Write
        .with_post_edit_hook(f.checker(Some("warning")))
        .execute(json!({"path": f.file(), "content": "replacement"}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert!(!result.content.contains("comment-checker warning"));
    assert_eq!(
        result.detail.unwrap()["comment_checker"]["reason"],
        "previous_contents_unavailable"
    );
    assert_eq!(std::fs::read_to_string(f.file()).unwrap(), "replacement");
    assert!(f.sandbox.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn input_budget_covers_new_existing_edit_and_json_escaping_without_spawning() {
    use tools::post_edit::MAX_INPUT_BYTES;
    for kind in ["new", "existing", "edit", "escaped", "previous-too-large"] {
        let f = Fixture::new();
        let checker = f.checker(Some("warning"));
        let content = match kind {
            "escaped" => "\0".repeat(MAX_INPUT_BYTES / 3),
            "existing" | "edit" => "b".repeat(MAX_INPUT_BYTES / 2),
            "previous-too-large" => "replacement".into(),
            _ => "x".repeat(MAX_INPUT_BYTES + 1),
        };
        let previous = "a".repeat(if kind == "previous-too-large" {
            MAX_INPUT_BYTES + 1
        } else {
            MAX_INPUT_BYTES / 2
        });
        if matches!(kind, "existing" | "edit" | "previous-too-large") {
            std::fs::write(f.file(), &previous).unwrap();
        }
        let result = if kind == "edit" {
            Edit.with_post_edit_hook(checker)
                .execute(json!({"path": f.file(), "old_string": previous, "new_string": content}))
                .await
                .unwrap()
        } else {
            Write
                .with_post_edit_hook(checker)
                .execute(json!({"path": f.file(), "content": content}))
                .await
                .unwrap()
        };
        assert!(!result.is_error, "{kind}");
        assert!(!result.content.contains("comment-checker warning"));
        let expected = if kind == "previous-too-large" {
            "previous_contents_unavailable"
        } else {
            "input_too_large"
        };
        assert_eq!(
            result.detail.unwrap()["comment_checker"]["reason"],
            expected,
            "{kind}"
        );
        assert_eq!(std::fs::read_to_string(f.file()).unwrap(), content);
        assert!(f.sandbox.0.lock().unwrap().is_empty(), "{kind}");
    }
}

#[tokio::test(start_paused = true)]
async fn large_output_before_input_is_drained_and_stderr_retention_is_bounded() {
    let _clock = FrozenClock::new();
    let f = Fixture::new();
    let input = PostEditInput {
        content: Some("x".repeat(1024 * 1024)),
        ..f.input()
    };
    let outcome = f.checker(Some("large-output")).check(&input).await;
    let PostEditOutcome::Warning { message } = outcome else {
        panic!("unexpected outcome: {outcome:?}");
    };
    assert_eq!(
        message,
        format!(
            "{}\n[comment-checker stderr truncated after 65536 bytes]",
            "x".repeat(64 * 1024)
        )
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.read("checker.input")).unwrap()["tool_input"]
            ["content"]
            .as_str()
            .unwrap()
            .len(),
        1024 * 1024
    );
}

#[test]
fn resolver_rejects_forbidden_and_unverifiable_roots_before_sandbox_wrap() {
    let f = Fixture::new();
    assert!(
        CommentChecker::resolve_with_roots(
            f.sandbox.clone(),
            &f.config(None),
            &[f.dir.path().into()]
        )
        .is_none()
    );
    assert!(
        CommentChecker::resolve_with_roots(
            f.sandbox.clone(),
            &f.config(None),
            &[f.dir.path().join("missing")]
        )
        .is_none()
    );
    let mut config = f.config(None);
    config.binary = "./comment-checker".into();
    assert!(CommentChecker::resolve_with_roots(f.sandbox.clone(), &config, &[]).is_none());
    assert!(f.sandbox.0.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn drain_of_an_active_check_uses_the_original_deadline() {
    let _clock = FrozenClock::new();
    let f = Fixture::new();
    let handshakes = Handshakes::new(&f);
    let checker = f.checker(Some("timeout"));
    let check = checker.clone();
    let input = f.input();
    let task = tokio::spawn(async move { check.check(&input).await });
    let (leader, pid) = Handshakes::accept(&handshakes.leader).await;
    let (descendant, _) = Handshakes::accept(&handshakes.descendant).await;
    let drain = checker.drain();
    tokio::pin!(drain);
    std::future::poll_fn(|cx| {
        use std::future::Future;
        assert!(
            drain.as_mut().poll(cx).is_pending(),
            "drain must retain the active supervisor"
        );
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::advance(Duration::from_millis(15_001)).await;
    drain.await;
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert!(matches!(
        task.await.unwrap(),
        PostEditOutcome::Unavailable {
            reason: UnavailableReason::Timeout,
            ..
        }
    ));
    assert_closed(leader).await;
    assert_closed(descendant).await;
}

#[tokio::test]
async fn adapter_rejects_oversized_and_non_utf8_input_without_spawning() {
    use std::os::unix::ffi::OsStringExt;
    let f = Fixture::new();
    let checker = f.checker(None);
    let input = PostEditInput {
        content: Some("x".repeat(tools::post_edit::MAX_INPUT_BYTES + 1)),
        ..f.input()
    };
    assert!(matches!(
        checker.check(&input).await,
        PostEditOutcome::Unavailable {
            reason: UnavailableReason::InputTooLarge,
            ..
        }
    ));
    let input = PostEditInput {
        file_path: f.dir.path().join(std::ffi::OsString::from_vec(vec![0xff])),
        ..f.input()
    };
    assert!(matches!(
        checker.check(&input).await,
        PostEditOutcome::Unavailable {
            reason: UnavailableReason::Io,
            ..
        }
    ));
    checker.drain().await;
    assert!(f.sandbox.0.lock().unwrap().is_empty());
}
