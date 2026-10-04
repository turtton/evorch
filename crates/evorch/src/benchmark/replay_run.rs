use super::engine::{Lease, compose, local_metrics, terminal_outcome, wait_bounded};
use super::storage::{
    Baseline, Evaluation, Metrics, Trial, read_json, reject_git_metadata, skipped_evaluation,
    write_json,
};
use super::verifier::{VerificationContext, evaluate};
use super::{BenchmarkResult, invalid};
use crate::headless::SandboxChoice;
use event_bus::AgentRunPhase;
use routing::EnvLookup;
use runtime::ModelPreference;
use runtime::snapshot::{SnapshotId, SnapshotStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub async fn replay(
    directory: &Path,
    trial_name: &str,
    candidate: ModelPreference,
    user_config_dir: Option<PathBuf>,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
) -> BenchmarkResult<Trial> {
    if !super::spec::valid_trial(trial_name) {
        return Err(invalid("invalid trial name"));
    }
    let directory = directory.canonicalize()?;
    let _lease = Lease::acquire(&directory)?;
    let mut baseline: Baseline = read_json(&directory.join("baseline.json"))?;
    if baseline.poisoned.is_some() || directory.join("poisoned.json").exists() {
        return Err(invalid(
            "benchmark record is poisoned; create a new record instead of reusing the workspace",
        ));
    }
    reject_git_metadata(&baseline.workspace)?;
    baseline.spec.validate()?;
    if baseline.version != 1
        || baseline.workspace != directory.join("workspace")
        || baseline.workspace.canonicalize()? != baseline.workspace
    {
        return Err(invalid(
            "record moved or workspace replaced; absolute checkpoint paths must stay fixed",
        ));
    }
    let checkpoint = baseline
        .checkpoint
        .clone()
        .ok_or_else(|| invalid("baseline did not capture the selected delegation"))?;
    let before = SnapshotId::from_hex(
        baseline
            .checkpoint_snapshot
            .clone()
            .ok_or_else(|| invalid("missing checkpoint snapshot"))?,
    )?;
    let trials = directory.join("trials");
    std::fs::create_dir_all(&trials)?;
    let trial_dir = trials.join(trial_name);
    std::fs::create_dir(&trial_dir)?;
    write_json(&trial_dir.join("candidate.json"), &candidate)?;
    let mut store = SnapshotStore::open(&baseline.workspace, &directory.join("snapshots"))?;
    store.restore_all(&before)?;
    let verifier_fixture = directory.join("verifier-fixture");
    let verification = VerificationContext {
        spec: &baseline.spec,
        protected: &baseline.protected,
        original_files: &baseline.original_files,
        fixture: &verifier_fixture,
        sandbox,
    };
    let mut runtime_started = false;
    let mut candidate_metrics = Metrics::default();
    let attempt = async {
        let session = compose(
            &baseline.workspace,
            user_config_dir.or(baseline.user_config_dir.clone()),
            env,
            sandbox,
            checkpoint.role,
        )
        .await?;
        let run = session.runtime.replay_benchmark(
            checkpoint,
            baseline.workspace.clone(),
            candidate.clone(),
        )?;
        runtime_started = true;
        let (phase, error) =
            wait_bounded(&session.runtime, run, &baseline.spec, &session.token_usage).await;
        let final_text = session.runtime.run_result(run).ok().flatten();
        let events = session.finish_events().await?;
        write_json(&trial_dir.join("events.json"), &events)?;
        let (phase, error) = terminal_outcome(phase, error, &events, &baseline.spec, run);
        candidate_metrics = local_metrics(&events, Some(run));
        let (diff, evaluation) = if phase == AgentRunPhase::Done {
            reject_git_metadata(&baseline.workspace)?;
            let after = store.capture_all()?;
            (
                store.diff(&before, &after)?,
                evaluate(&baseline.workspace, &verification).await,
            )
        } else {
            (String::new(), skipped_evaluation())
        };
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Trial {
            name: trial_name.into(),
            candidate: Some(candidate.clone()),
            phase,
            final_text,
            error,
            diff,
            metrics: candidate_metrics.clone(),
            evaluation,
        })
    }
    .await;
    let trial = attempt.unwrap_or_else(|error| Trial {
        name: trial_name.into(),
        candidate: Some(candidate),
        phase: AgentRunPhase::Error,
        final_text: None,
        error: Some(error.to_string()),
        diff: String::new(),
        metrics: candidate_metrics,
        evaluation: Evaluation {
            passed: false,
            output: "execution failed before verification".into(),
            verifier_tampered: vec![],
            unexpected_changes: vec![],
        },
    });
    if runtime_started && trial.phase != AgentRunPhase::Done {
        let reason = trial
            .error
            .clone()
            .unwrap_or_else(|| "candidate did not complete; process teardown is not proven".into());
        write_json(
            &directory.join("poisoned.json"),
            &serde_json::json!({"reason":reason}),
        )?;
        baseline.poisoned = Some(reason);
        write_json(&directory.join("baseline.json"), &baseline)?;
    }
    write_json(&trial_dir.join("trial.json"), &trial)?;
    super::report::save_report(&directory)?;
    Ok(trial)
}
