use super::engine::{Lease, compose, local_metrics, millis, terminal_outcome, wait_bounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use event_bus::AgentRunPhase;
use routing::EnvLookup;
use runtime::snapshot::SnapshotStore;
use runtime::{Role, RunConfig};

use super::storage::{
    Baseline, CaptureState, Metrics, Recorder, Trial, copy_fixture, file_inventory,
    protected_files, reject_git_metadata, skipped_evaluation, write_json,
};
use super::verifier::{VerificationContext, evaluate};
use super::{BenchmarkResult, TaskSpec, invalid};
use crate::headless::SandboxChoice;

pub async fn record(
    spec_path: &Path,
    output: &Path,
    user_config_dir: Option<PathBuf>,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
) -> BenchmarkResult<Baseline> {
    let existed = output.exists();
    let result = record_inner(spec_path, output, user_config_dir, env, sandbox).await;
    if !existed
        && output.is_dir()
        && let Err(error) = &result
    {
        write_json(
            &output.join("failure.json"),
            &serde_json::json!({"status":"error", "error":error.to_string()}),
        )?;
    }
    result
}

async fn record_inner(
    spec_path: &Path,
    output: &Path,
    user_config_dir: Option<PathBuf>,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
) -> BenchmarkResult<Baseline> {
    let spec = TaskSpec::load(spec_path)?;
    let user_config_dir = user_config_dir
        .map(|dir| {
            if dir.is_absolute() {
                Ok(dir)
            } else {
                std::env::current_dir().map(|cwd| cwd.join(dir))
            }
        })
        .transpose()?;
    // Refuse reuse and nesting: a run never edits the supplied fixture.
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let absolute_output = parent.canonicalize()?.join(
        output
            .file_name()
            .ok_or_else(|| invalid("output needs a directory name"))?,
    );
    if absolute_output.starts_with(&spec.fixture_dir)
        || spec.fixture_dir.starts_with(&absolute_output)
    {
        return Err(invalid("benchmark output must be outside the fixture"));
    }
    std::fs::create_dir(&absolute_output)?;
    let _lease = Lease::acquire(&absolute_output)?;
    let workspace = absolute_output.join("workspace");
    copy_fixture(&spec.fixture_dir, &workspace)?;
    let protected = protected_files(&spec, &workspace)?;
    let original_files = file_inventory(&workspace)?;
    let verifier_fixture = absolute_output.join("verifier-fixture");
    copy_fixture(&workspace, &verifier_fixture)?;
    let verification = VerificationContext {
        spec: &spec,
        protected: &protected,
        original_files: &original_files,
        fixture: &verifier_fixture,
        sandbox,
    };
    let mut store = SnapshotStore::open(&workspace, &absolute_output.join("snapshots"))?;
    let initial = store.capture_all()?;
    write_json(&absolute_output.join("spec.json"), &spec)?;
    let recorder = Arc::new(Recorder {
        state: Mutex::new(CaptureState {
            store,
            checkpoint: None,
            snapshot: None,
            local: None,
        }),
        directory: absolute_output.clone(),
    });
    let session = compose(
        &workspace,
        user_config_dir.clone(),
        env,
        sandbox,
        Role::Orchestrator,
    )
    .await?;
    let runtime = session
        .runtime
        .clone()
        .with_benchmark_recorder(spec.target.clone(), recorder.clone())?;
    let budget = runtime::budget_tracker::BudgetSettings {
        max_tokens: Some(spec.budget.max_tokens),
        max_tool_calls: spec.budget.max_tool_calls,
        max_elapsed: Duration::from_secs(spec.budget.timeout_seconds),
        ..Default::default()
    };
    let run = runtime.delegate_background(
        Role::Orchestrator,
        spec.root_prompt.clone(),
        RunConfig {
            budget,
            ..Default::default()
        },
    );
    let started = Instant::now();
    let (phase, error) = wait_bounded(&runtime, run, &spec, &session.token_usage).await;
    let incomplete: Vec<_> = runtime
        .list_agents()
        .into_iter()
        .filter(|agent| agent.phase != AgentRunPhase::Done)
        .collect();
    let incomplete_reason = (!incomplete.is_empty()).then(|| {
        format!(
            "baseline includes incomplete agents: {}",
            incomplete
                .iter()
                .map(|agent| format!("{} {:?}", agent.run_id, agent.phase))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    if incomplete.iter().any(|agent| {
        matches!(
            agent.phase,
            AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
        )
    }) {
        let _ = runtime.cancel_subtree(run);
        for agent in &incomplete {
            let _ = runtime.wait(agent.run_id).await;
        }
    }
    let events = session.finish_events().await?;
    write_json(&absolute_output.join("baseline-events.json"), &events)?;
    let (phase, error) = terminal_outcome(phase, error, &events, &spec, run);
    let full_elapsed = millis(started.elapsed());
    let (checkpoint, checkpoint_snapshot, local, poisoned, final_snapshot) = {
        let mut state = recorder.state.lock().map_err(|e| invalid(e.to_string()))?;
        let reason = if phase != AgentRunPhase::Done {
            Some(
                error
                    .clone()
                    .unwrap_or_else(|| "baseline execution did not complete".into()),
            )
        } else if let Some(reason) = incomplete_reason {
            Some(reason)
        } else if state
            .local
            .as_ref()
            .is_some_and(|(phase, _, _)| *phase != AgentRunPhase::Done)
            || (state.checkpoint.is_some() && state.local.is_none())
            || absolute_output.join("poisoned.json").exists()
        {
            Some("selected delegation did not complete; process teardown is not proven".into())
        } else {
            None
        };
        let snapshot = if reason.is_none() {
            reject_git_metadata(&workspace)?;
            Some(state.store.capture_all()?)
        } else {
            None
        };
        (
            state.checkpoint.clone(),
            state.snapshot.clone(),
            state.local.clone(),
            reason,
            snapshot,
        )
    };
    if let Some(reason) = &poisoned {
        write_json(
            &absolute_output.join("poisoned.json"),
            &serde_json::json!({"reason":reason}),
        )?;
    }
    let full_diff = if let Some(final_snapshot) = &final_snapshot {
        recorder
            .state
            .lock()
            .map_err(|e| invalid(e.to_string()))?
            .store
            .diff(&initial, final_snapshot)?
    } else {
        String::new()
    };
    let full_evaluation = if poisoned.is_none() {
        evaluate(&workspace, &verification).await
    } else {
        skipped_evaluation()
    };
    let full = Trial {
        name: "baseline-full".into(),
        candidate: None,
        phase,
        final_text: runtime.run_result(run).ok().flatten(),
        error: error.or_else(|| {
            (checkpoint.is_none()).then(|| "selected delegation was not captured; inspect baseline-events.json for unsupported execution or missing target".into())
        }),
        diff: full_diff,
        metrics: Metrics::from_events(&events, full_elapsed),
        evaluation: full_evaluation,
    };
    let local = if let Some((local_phase, final_text, snapshot)) = local {
        let local_run = checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.origin_run_id);
        let local_error = if local_phase != AgentRunPhase::Done {
            local_run.and_then(|run| terminal_outcome(local_phase, None, &events, &spec, run).1)
        } else {
            None
        };
        let (diff, evaluation) = if poisoned.is_none() {
            if let (Some(snapshot), Some(before), Some(final_snapshot)) =
                (&snapshot, &checkpoint_snapshot, &final_snapshot)
            {
                let diff = {
                    let mut state = recorder.state.lock().map_err(|e| invalid(e.to_string()))?;
                    let diff = state.store.diff(before, snapshot)?;
                    state.store.restore_all(snapshot)?;
                    diff
                };
                let evaluation = evaluate(&workspace, &verification).await;
                recorder
                    .state
                    .lock()
                    .map_err(|e| invalid(e.to_string()))?
                    .store
                    .restore_all(final_snapshot)?;
                (diff, evaluation)
            } else {
                (String::new(), skipped_evaluation())
            }
        } else {
            (String::new(), skipped_evaluation())
        };
        Some(Trial {
            name: "baseline-local".into(),
            candidate: None,
            phase: local_phase,
            final_text,
            error: local_error,
            diff,
            metrics: local_metrics(&events, local_run),
            evaluation,
        })
    } else {
        None
    };
    let baseline = Baseline {
        version: 1,
        spec,
        workspace,
        user_config_dir,
        initial_snapshot: initial.as_str().into(),
        checkpoint_snapshot: checkpoint_snapshot.map(|id| id.as_str().into()),
        checkpoint,
        local,
        full,
        protected,
        original_files,
        poisoned,
    };
    write_json(&absolute_output.join("baseline.json"), &baseline)?;
    super::report::save_report(&absolute_output)?;
    Ok(baseline)
}
