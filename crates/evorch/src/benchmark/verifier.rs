use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use event_bus::EventBus;
use sandbox::{BwrapConfig, BwrapSandbox, DirectSandbox};
use tools::{ToolExecutionContext, ToolExecutor};

use super::storage::{Evaluation, copy_fixture, file_inventory, tampered_files};
use super::{BenchmarkResult, TaskSpec, invalid};
use crate::headless::SandboxChoice;

pub(super) struct VerificationContext<'a> {
    pub spec: &'a TaskSpec,
    pub protected: &'a BTreeMap<PathBuf, Vec<u8>>,
    pub original_files: &'a BTreeMap<PathBuf, Vec<u8>>,
    pub fixture: &'a Path,
    pub sandbox: SandboxChoice,
}

pub(super) async fn evaluate(workspace: &Path, context: &VerificationContext<'_>) -> Evaluation {
    let tampered = tampered_files(context.protected, workspace);
    if !tampered.is_empty() {
        return Evaluation {
            passed: false,
            output: "trusted verifier files changed; verification rejected".into(),
            verifier_tampered: tampered,
            unexpected_changes: vec![],
        };
    }
    let result = evaluate_inner(workspace, context).await;
    match result {
        Ok(evaluation) => evaluation,
        Err(error) => Evaluation {
            passed: false,
            output: error.to_string(),
            verifier_tampered: vec![],
            unexpected_changes: vec![],
        },
    }
}

async fn evaluate_inner(
    workspace: &Path,
    context: &VerificationContext<'_>,
) -> BenchmarkResult<Evaluation> {
    let current = file_inventory(workspace)?;
    let paths: BTreeSet<_> = current
        .keys()
        .chain(context.original_files.keys())
        .collect();
    let unexpected: Vec<_> = paths
        .into_iter()
        .filter(|path| {
            current.get(*path) != context.original_files.get(*path)
                && !context.spec.verifier.allowed_changed_paths.contains(path)
        })
        .cloned()
        .collect();
    if !unexpected.is_empty() {
        return Ok(Evaluation {
            passed: false,
            output: "changes outside allowed implementation artifacts; verification rejected"
                .into(),
            verifier_tampered: vec![],
            unexpected_changes: unexpected,
        });
    }
    // Candidate build caches and artifacts never enter verification. Import only
    // explicit output files into an untouched copy of the original harness.
    let parent = context
        .fixture
        .parent()
        .ok_or_else(|| invalid("verifier fixture has no parent"))?;
    let directory = tempfile::tempdir_in(parent)?;
    let verify_workspace = directory.path().join("workspace");
    copy_fixture(context.fixture, &verify_workspace)?;
    for path in &context.spec.verifier.allowed_changed_paths {
        let target = verify_workspace.join(path);
        if let Some(bytes) = current.get(path) {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, bytes)?;
        } else if target.is_file() {
            std::fs::remove_file(target)?;
        }
    }
    let bus = Arc::new(EventBus::new(32));
    let sandbox: Arc<dyn sandbox::Sandbox> = match context.sandbox {
        SandboxChoice::Production => Arc::new(BwrapSandbox::detect(
            BwrapConfig::new(verify_workspace.clone())
                .isolate_processes()
                .allow_network(false),
        )?),
        SandboxChoice::DirectUnchecked => Arc::new(DirectSandbox::new_unchecked()),
    };
    let executor =
        ToolExecutor::with_standard_tools_in(bus, sandbox, Some(verify_workspace.clone()));
    executor.set_workspace_boundary(verify_workspace.clone())?;
    let command = std::iter::once(&context.spec.verifier.program)
        .chain(context.spec.verifier.args.iter())
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let ctx = ToolExecutionContext {
        run_id: "benchmark-verifier".into(),
        shell_wait_policy: tools::executor::ShellWaitPolicy::UntilTerminal,
        ..Default::default()
    };
    let args = serde_json::json!({
        "command": command,
        "cwd": verify_workspace,
        "timeout_ms": context.spec.budget.timeout_seconds.saturating_mul(1000),
    });
    let mut result = executor
        .execute(&ctx, "shell", "benchmark-verifier", args)
        .await?;
    if result
        .detail
        .as_ref()
        .and_then(|d| d["shell_job"]["status"].as_str())
        == Some("running")
    {
        return Err(invalid("verifier shell violated UntilTerminal contract"));
    }
    if let Some(path) = result
        .detail
        .as_ref()
        .and_then(|detail| detail["output_artifact"]["path"].as_str())
    {
        let source = Path::new(path).canonicalize()?;
        if !source.starts_with(&verify_workspace) {
            return Err(invalid("verifier artifact escaped its private workspace"));
        }
        let artifacts = parent.join("verifier-artifacts");
        std::fs::create_dir_all(&artifacts)?;
        let saved = tempfile::Builder::new()
            .prefix("verifier-")
            .suffix(".log")
            .tempfile_in(artifacts)?;
        std::fs::copy(&source, saved.path())?;
        let (_, saved_path) = saved.keep()?;
        // No model has received this independent verifier result. Publish its
        // durable reference once; later reports keep that reference unchanged.
        result.content = result.content.replace(path, &saved_path.to_string_lossy());
    }
    Ok(Evaluation {
        passed: !result.is_error,
        output: result.content,
        verifier_tampered: vec![],
        unexpected_changes: vec![],
    })
}

#[cfg(test)]
mod auto_yield_contract_tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn evaluator_waits_past_ten_seconds_then_rejects_exit_seven() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("fixture");
        std::fs::create_dir(&fixture).unwrap();
        let started = directory.path().join("started");
        let release = directory.path().join("release");
        std::fs::write(
            fixture.join("verify.sh"),
            format!(
                "echo started > '{}'; while ! test -e '{}'; do sleep 0.01; done; exit 7",
                started.display(),
                release.display(),
            ),
        )
        .unwrap();
        let original = file_inventory(&fixture).unwrap();
        let spec: TaskSpec = serde_json::from_value(serde_json::json!({
            "fixture_dir":fixture, "root_prompt":"verify", "target":{"role":"worker","category":null,"occurrence":1},
            "verifier":{"program":"sh","args":["verify.sh"],"protected_paths":["verify.sh"],"allowed_changed_paths":[]},
            "budget":{"max_tokens":1000,"max_tool_calls":10,"timeout_seconds":60}
        })).unwrap();
        let context = VerificationContext {
            spec: &spec,
            protected: &original,
            original_files: &original,
            fixture: &fixture,
            sandbox: SandboxChoice::DirectUnchecked,
        };
        let mut evaluating = std::pin::pin!(evaluate(&fixture, &context));
        std::future::poll_fn(|cx| {
            assert!(evaluating.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        for _ in 0..2000 {
            tokio::task::yield_now().await;
            if started.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(started.exists());
        tokio::time::advance(Duration::from_secs(20)).await;
        std::future::poll_fn(|cx| {
            assert!(evaluating.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        std::fs::write(release, "").unwrap();
        tokio::time::resume();
        let evaluation = evaluating.await;
        assert!(!evaluation.passed);
        assert!(evaluation.output.starts_with("exit_code: 7\n"));
    }
}
