//! L12 uses the product Evaluator entrance. Cell `eval` replaces the external
//! CLI with a fixture; cell `eval_cli` runs the real `evorch eval run`.
use super::observer::{ProcessTree, bounded};
use mock_openai::ScriptedResponse;
use serde_json::json;
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use workbench_client::WorkbenchClient;
use workbench_protocol::{
    commands::{ApprovalDecision, Command, ComposerRole, DecideToolApproval, SendChat},
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::*,
    namespaces::{
        ConversationCommand,
        approvals::{ApprovalsCommand, ApprovalsEvent},
        execution::ExecutionEvent,
    },
    views::{ApprovalState, RunPhase},
};
const LIMIT: Duration = Duration::from_secs(30);
fn error(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{e:?}"))
}
pub async fn repository(project: &Path) -> io::Result<()> {
    bounded(LIMIT, "sample repository", async {
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            let status = tokio::process::Command::new("git")
                .args(args)
                .current_dir(project)
                .status()
                .await?;
            if !status.success() {
                return Err(error(status));
            }
        }
        // Executable/socket fixtures are external input, not replayable source.
        std::fs::write(project.join(".git/info/exclude"), "*\n")?;
        Ok(())
    })
    .await
}
/// Under the core's HOME (the test's `root`): the sandbox mounts
/// `$HOME/.cargo/bin` read-only at `/tmp/home/.cargo/bin`.
pub const REPLAY_PEER_DIR: &str = ".cargo/bin";
const SANDBOX_PEER_DIR: &str = "/tmp/home/.cargo/bin";
/// Where the replay's shell sees `host`, a file in [`REPLAY_PEER_DIR`].
pub fn in_replay_sandbox(root: &Path, host: &Path) -> PathBuf {
    Path::new(SANDBOX_PEER_DIR).join(host.strip_prefix(root.join(REPLAY_PEER_DIR)).unwrap())
}
/// Index of the replay's first agent request in `eval_cli`'s script: the
/// requests before it are the sample run, the orchestrator's delegate, the
/// evaluator's save, start and final text, and the orchestrator's final text.
pub const REPLAY_REQUEST: usize = 6;
/// `replay_peer` is the shell command the replay's first agent request asks
/// the product shell tool to run (cell `eval_cli`); `None` keeps cell `eval`.
pub fn responses(replay_peer: Option<&str>) -> Vec<ScriptedResponse> {
    let call = |id: &str, name: &str, args: serde_json::Value| {
        ScriptedResponse::tool_call(id, "mock-model", 0, id, name, [args.to_string()])
    };
    let mut script = vec![
        ScriptedResponse::text_stream("sample", "mock-model", ["recorded sample"]),
        call(
            "delegate",
            "delegate",
            json!({"target":{"role":"evaluator"},"prompt":"Pin sample thread sample then start the candidate evaluation"}),
        ),
        call(
            "save",
            "eval_save_sample",
            json!({"thread_id":"sample","problem":"matrix fixture","focus_roles":["Worker"]}),
        ),
        call(
            "start",
            "eval_start",
            json!({"thread_id":"sample","candidate":"[[routing.routes.worker]]\nprofile = \"candidate\"\n","repeat":1}),
        ),
        ScriptedResponse::text_stream("done", "mock-model", ["evaluation launched"]),
    ];
    if let Some(peer) = replay_peer {
        script.push(ScriptedResponse::text_stream(
            "orchestrator-done",
            "mock-model",
            ["delegated"],
        ));
        script.push(call(
            "replay-shell",
            "shell",
            json!({"command":peer,"timeout_ms":120000}),
        ));
        debug_assert_eq!(script.len() - 1, REPLAY_REQUEST);
    }
    script
}
fn input(thread: &str, role: ComposerRole) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(thread).unwrap(),
        composer_role: role,
        text: "run fixture".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}
pub async fn enter(client: &WorkbenchClient) -> io::Result<()> {
    let mut events = client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .map_err(error)?;
    client
        .command(Command::Conversation(ConversationCommand::CreateExecution(
            input("sample", ComposerRole::Worker),
        )))
        .await
        .map_err(error)?;
    bounded(LIMIT, "sample run completion", async {
        while let Some(event) = events.next().await {
            if let UiEvent::Execution(ExecutionEvent::RunChanged { run }) = event.event
                && run.thread.as_str() == "sample"
            {
                if matches!(run.phase, RunPhase::Done | RunPhase::WaitingForUser) {
                    return Ok(());
                }
                if matches!(run.phase, RunPhase::Failed | RunPhase::Cancelled) {
                    return Err(error(run));
                }
            }
        }
        Err(io::Error::other("sample events closed"))
    })
    .await?;
    client
        .command(Command::Conversation(ConversationCommand::CreateExecution(
            input("matrix", ComposerRole::Orchestrator),
        )))
        .await
        .map_err(error)?;
    bounded(LIMIT, "eval_start approval", async {
        while let Some(event) = events.next().await {
            if let UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) = event.event
                && approval.tool_name == "eval_start"
                && approval.state == ApprovalState::Pending
            {
                client
                    .command(Command::Approvals(ApprovalsCommand::DecideToolApproval(
                        DecideToolApproval {
                            approval_id: approval.id,
                            core_epoch: approval.core_epoch,
                            approval_revision: approval.revision,
                            decision: ApprovalDecision::Approve,
                        },
                    )))
                    .await
                    .map_err(error)?;
                return Ok(());
            }
        }
        Err(io::Error::other("eval approval events closed"))
    })
    .await
}
/// The flags the product launch passes to `evorch eval run`.
fn check_argv(args: &[String]) -> io::Result<()> {
    for expected in [["eval", "run"], ["--thread", "sample"], ["--repeat", "1"]] {
        if !args.windows(2).any(|pair| pair == expected) {
            return Err(error((&expected, args)));
        }
    }
    for flag in ["--candidate", "--problem", "--output", "--user-config"] {
        if !args.iter().any(|arg| arg == flag) {
            return Err(error((flag, args)));
        }
    }
    Ok(())
}
pub fn verify_argv(project: &Path) -> io::Result<()> {
    let text = std::fs::read_to_string(project.join(".eval-argv"))?;
    let args: Vec<_> = text.lines().map(str::to_owned).collect();
    check_argv(&args)?;
    super::evidence::emit(
        "eval",
        "argv",
        &json!({"cell":"eval","external_cli_fixture":true,"argv":args,"core_caller":"groups::launch -> spawn_owned(ChildKind::Eval)"}),
    )?;
    Ok(())
}

/// Installs the real `evorch` CLI the way a distribution lays it out: the ELF,
/// the guard bundle that binds it (role `cli`) with its helper, and the
/// sibling `evorch-core` the launcher insists on finding. The bundle is the
/// one the build's bind step wrote beside the CLI; it is never made up here.
pub fn install_cli(bindir: &Path) -> io::Result<PathBuf> {
    let source = PathBuf::from(
        std::env::var_os("EVORCH_B3_CLI_EXE")
            .ok_or_else(|| io::Error::other("EVORCH_B3_CLI_EXE (actual evorch CLI) required"))?,
    );
    let dir = source
        .parent()
        .ok_or_else(|| io::Error::other("CLI has no directory"))?;
    let manifest = dir.join("guard-bundle.json");
    let bundle: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest).map_err(
        |e| {
            io::Error::other(format!(
                "{}: {e}; the CLI needs its installed guard bundle (scripts/guard-cargo.sh bind-callers writes it beside the build)",
                manifest.display()
            ))
        },
    )?)?;
    let helper = bundle["helper"]["file"]
        .as_str()
        .ok_or_else(|| io::Error::other("bundle has no helper file"))?;
    let target = bindir.join("evorch");
    std::fs::copy(&source, &target)?;
    let sha = super::evidence::digest(&target)?;
    super::product::check(
        bundle["callers"].as_array().is_some_and(|callers| {
            callers.iter().any(|caller| {
                caller["role"] == "cli" && caller["file"] == "evorch" && caller["sha256"] == sha
            })
        }),
        "the bundle beside EVORCH_B3_CLI_EXE does not bind this CLI as role cli",
    )?;
    std::fs::copy(&manifest, bindir.join("guard-bundle.json"))?;
    let helper_copy = bindir.join(helper);
    std::fs::create_dir_all(helper_copy.parent().unwrap())?;
    std::fs::copy(dir.join(helper), &helper_copy)?;
    let artifact = test_support::product_artifact::require_from_env();
    std::fs::copy(&artifact.exe, bindir.join("evorch-core"))?;
    Ok(target)
}

fn arg0_is(pid: i32, path: &Path) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|bytes| {
        bytes.split(|b| *b == 0).next() == Some(path.as_os_str().as_encoded_bytes())
    })
}

/// What the real CLI is, observed once the replay's own descendant is READY:
/// a thin client under the core's guard that owns no process. Returns the
/// CLI's pid, its guard and the processes of the CLI's group.
pub fn observe_cli(
    tree: &ProcessTree,
    cli: &Path,
    core_copy: &Path,
    requests: &[mock_openai::server::RecordedRequest],
) -> io::Result<(i32, serde_json::Value, Vec<i32>)> {
    use super::product::{check, guard_owner};
    let clis: Vec<_> = tree
        .descendants
        .keys()
        .copied()
        .filter(|pid| {
            arg0_is(*pid, cli)
                && std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|bytes| bytes.split(|b| *b == 0).nth(1) == Some(b"eval".as_slice()))
        })
        .collect();
    check(
        clis.len() == 1,
        format!("expected exactly one live real evorch eval run, found {clis:?}"),
    )?;
    let pid = clis[0];
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .take_while(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    check_argv(&args[1..])?;
    let identity = &tree.require(pid)?.identity;
    check(
        !tree.descendants.values().any(|p| p.identity.parent == pid),
        "evorch eval run is a client of the running core and must own no process",
    )?;
    check(
        !tree.descendants.keys().any(|p| arg0_is(*p, core_copy)),
        "the CLI started a core of its own instead of attaching to the running one",
    )?;
    let group: Vec<_> = tree
        .descendants
        .values()
        .filter(|p| p.identity.group == identity.group)
        .map(|p| p.identity.pid)
        .collect();
    let guard = guard_owner(tree, pid)?;
    check(
        guard["target_root"].as_i64() == Some(i64::from(pid)),
        "evorch eval run must be the guard's root process itself, not a descendant of a wrapper",
    )?;
    // The shell call in the script went to the replay: only replay requests
    // carry the run identifier preamble. Script entries are consumed by chat
    // requests only (the model listing is not one).
    let chat: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/chat/completions"))
        .collect();
    let replay = |index: usize| {
        chat.get(index)
            .is_some_and(|request| request.body.to_string().contains("Run identifier: "))
    };
    check(
        replay(REPLAY_REQUEST) && (0..REPLAY_REQUEST).all(|index| !replay(index)),
        format!(
            "script request {REPLAY_REQUEST} was not the replay's first request ({} chat requests recorded)",
            chat.len()
        ),
    )?;
    Ok((pid, guard, group))
}
