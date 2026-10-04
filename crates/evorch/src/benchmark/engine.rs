use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{AgentRunPhase, Event, EventBus};
use routing::EnvLookup;
use runtime::{
    AgentRuntime, ExecutionPolicy, ModelSource, Role, RunId, RuntimeComposition, compose_runtime,
    production_executor,
};
use sandbox::DirectSandbox;
use tools::ToolExecutor;

use super::storage::Metrics;
use super::{BenchmarkResult, TaskSpec, invalid};
use crate::headless::SandboxChoice;

#[derive(Clone, Copy, Default)]
pub(super) struct BudgetUsage {
    tokens: u64,
    tool_calls: u32,
}

pub(super) struct Session {
    pub runtime: AgentRuntime,
    events: Arc<Mutex<Vec<Event>>>,
    pub token_usage: tokio::sync::watch::Receiver<BudgetUsage>,
    stop_events: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Session {
    pub async fn finish_events(&self) -> BenchmarkResult<Vec<Event>> {
        if let Some(sender) = self
            .stop_events
            .lock()
            .map_err(|e| invalid(e.to_string()))?
            .take()
        {
            let _ = sender.send(());
        }
        let task = self
            .event_task
            .lock()
            .map_err(|e| invalid(e.to_string()))?
            .take();
        if let Some(task) = task {
            task.await?;
        }
        Ok(self
            .events
            .lock()
            .map_err(|e| invalid(e.to_string()))?
            .clone())
    }
}

pub(super) async fn compose(
    workspace: &Path,
    user_config_dir: Option<PathBuf>,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
    role: Role,
) -> BenchmarkResult<Session> {
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(workspace.into()),
        user_config_dir: user_config_dir.clone(),
        read_env: false,
        ..Default::default()
    })?;
    let bus = Arc::new(EventBus::new(65536));
    let executor = match sandbox {
        SandboxChoice::Production => production_executor(
            bus.clone(),
            &ExecutionPolicy::for_role(role),
            workspace.into(),
        )?,
        SandboxChoice::DirectUnchecked => Arc::new(ToolExecutor::with_standard_tools_in(
            bus.clone(),
            Arc::new(DirectSandbox::new_unchecked()),
            Some(workspace.into()),
        )),
    };
    executor.set_workspace_boundary(workspace.into())?;
    let credential_dir = user_config_dir
        .clone()
        .or_else(config::user_config_dir)
        .ok_or_else(|| invalid("pass --user-config for credentials"))?
        .join("credentials");
    let credentials =
        tokio::task::spawn_blocking(move || sandbox::credential::open_default(credential_dir))
            .await??;
    let runtime = compose_runtime(RuntimeComposition {
        config: &config,
        user_config_dir: user_config_dir.clone(),
        bus: bus.clone(),
        executor: executor.clone(),
        credential_store: credentials,
        env,
        model_source: ModelSource::Configured,
        workspace: None,
    })?
    .runtime;
    let runtime = match sandbox {
        SandboxChoice::Production => runtime.with_sandbox_root(workspace.into()),
        SandboxChoice::DirectUnchecked => runtime,
    };
    runtime.set_default_cwd(workspace.into())?;
    // This initial benchmark has no external information tools.
    runtime.set_web_tools_enabled(false);
    runtime.set_sandbox_escalation(config::EscalationApproval::Off, false);
    let mut receiver = bus.subscribe();
    let events = Arc::new(Mutex::new(Vec::new()));
    let collected = events.clone();
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let (usage_tx, token_usage) = tokio::sync::watch::channel(BudgetUsage::default());
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                event = receiver.recv() => match event {
                    Ok(event) => {
                        if let event_bus::EventKind::Usage(event_bus::UsageEvent::Usage { input_tokens, output_tokens, .. }) = &event.kind {
                            usage_tx.send_modify(|total| total.tokens = total.tokens.saturating_add(*input_tokens).saturating_add(*output_tokens));
                        }
                        if matches!(&event.kind, event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted { .. })) {
                            usage_tx.send_modify(|total| total.tool_calls = total.tool_calls.saturating_add(1));
                        }
                        collected.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
                    },
                    Err(event_bus::RecvError::Lagged(count)) => collected.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Event::new(event_bus::FaultEvent::SubscriberLagged { subscriber_id: receiver.subscriber_id(), skipped: count })),
                    Err(event_bus::RecvError::Closed) => break,
                },
                _ = &mut stopped => { collected.lock().unwrap_or_else(std::sync::PoisonError::into_inner).extend(receiver.drain_pending_snapshot()); break; }
            }
        }
    });
    Ok(Session {
        runtime,
        events,
        token_usage,
        stop_events: Mutex::new(Some(stop)),
        event_task: Mutex::new(Some(task)),
    })
}

pub(super) async fn wait_bounded(
    runtime: &AgentRuntime,
    run: RunId,
    spec: &TaskSpec,
    token_usage: &tokio::sync::watch::Receiver<BudgetUsage>,
) -> (AgentRunPhase, Option<String>) {
    let mut usage = token_usage.clone();
    let wait = runtime.wait(run);
    tokio::pin!(wait);
    let deadline = tokio::time::sleep(Duration::from_secs(spec.budget.timeout_seconds));
    tokio::pin!(deadline);
    let reason = loop {
        let observed = *usage.borrow_and_update();
        if observed.tool_calls > spec.budget.max_tool_calls {
            break "benchmark tool-call budget exhausted";
        }
        if observed.tokens > spec.budget.max_tokens {
            break "benchmark token budget exhausted";
        }
        tokio::select! {
            result = &mut wait => return match result {
                Ok(phase) => (phase, None),
                Err(error) => (AgentRunPhase::Error, Some(error.to_string())),
            },
            _ = &mut deadline => break "benchmark wall-clock budget exhausted",
            result = usage.changed() => if result.is_err() { break "benchmark accounting channel closed"; },
        }
    };
    let _ = runtime.cancel_subtree(run);
    for agent in runtime.list_agents() {
        let _ = runtime.wait(agent.run_id).await;
    }
    (AgentRunPhase::Stopped, Some(reason.into()))
}

pub(super) fn local_metrics(events: &[Event], run: Option<RunId>) -> Metrics {
    let mut metrics = Metrics::default();
    let Some(run) = run.map(|id| id.to_string()) else {
        return metrics;
    };
    // Correlated request observations contain a copy of canonical usage; this
    // separate per-run subtotal is never added to whole-task UsageEvent totals.
    for event in events {
        if let event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
            run_id,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            duration_ms,
            ..
        }) = &event.kind
            && run_id.as_deref() == Some(&run)
        {
            metrics.input_tokens += input_tokens;
            metrics.output_tokens += output_tokens;
            metrics.cache_read_tokens += cache_read_tokens;
            metrics.cache_write_tokens += cache_write_tokens;
            metrics.elapsed_ms += duration_ms;
        }
    }
    metrics
}

/// The terminal event can beat asynchronous accounting notifications. Recheck
/// the complete drained trace before allowing any snapshot or verification.
pub(super) fn terminal_outcome(
    phase: AgentRunPhase,
    error: Option<String>,
    events: &[Event],
    spec: &TaskSpec,
    run: RunId,
) -> (AgentRunPhase, Option<String>) {
    let metrics = Metrics::from_events(events, 0);
    let tools = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted { .. })
            )
        })
        .count();
    if metrics.input_tokens.saturating_add(metrics.output_tokens) > spec.budget.max_tokens {
        return (
            AgentRunPhase::Stopped,
            Some("benchmark token budget exhausted".into()),
        );
    }
    if tools > spec.budget.max_tool_calls as usize {
        return (
            AgentRunPhase::Stopped,
            Some("benchmark tool-call budget exhausted".into()),
        );
    }
    let reason = events.iter().rev().find_map(|event| match &event.kind {
        event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) if run_id == &run.to_string() && *to == phase => reason.clone(),
        _ => None,
    });
    (phase, error.or(reason))
}

pub(super) fn millis(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub(super) struct Lease(PathBuf);
impl Lease {
    pub fn acquire(root: &Path) -> BenchmarkResult<Self> {
        let path = root.join("active.lock");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                invalid(format!(
                    "benchmark already active or interrupted ({}): {error}",
                    path.display()
                ))
            })?;
        Ok(Self(path))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(task) = self.event_task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}
