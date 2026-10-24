//! Runtime-only thread facts; GUI definitions remain authoritative (ADR 0041).
use storage::threads::{ThreadLineageKind, ThreadUpsert};

use crate::{AgentRuntime, ChatForkSeed, Role, RunConfig, RunId};

/// Resolve before a handoff removes the source's live thread binding.
pub(crate) fn source_thread(runtime: &AgentRuntime, source: RunId) -> Option<String> {
    if let Some(store) = runtime.shared.run_store.get() {
        match store.thread_of_run(&source.to_string()) {
            Ok(Some(binding)) => return Some(binding.thread_id),
            Ok(None) => {}
            Err(error) => tracing::warn!(%source, %error, "source thread could not be read"),
        }
    }
    runtime.goal_thread(source)
}

pub(crate) fn record_chat_root(
    runtime: &AgentRuntime,
    thread_id: &str,
    run_id: RunId,
    role: Role,
    config: &RunConfig,
    seed: Option<&ChatForkSeed>,
) {
    let mut row = definition(runtime, thread_id.to_owned(), run_id, role, config);
    if let Some(seed) = seed {
        row.parent_thread_id = runtime
            .shared
            .run_store
            .get()
            .and_then(|store| match store.thread_of_run(&seed.source_run_id) {
                Ok(binding) => binding.map(|binding| binding.thread_id),
                Err(error) => {
                    tracing::warn!(%error, "fork source thread could not be read");
                    None
                }
            })
            .or_else(|| {
                seed.source_run_id
                    .parse()
                    .ok()
                    .and_then(|run| runtime.goal_thread(run))
            });
        row.fork_point_run_id = Some(seed.source_run_id.clone());
        row.fork_point_context_len = Some(seed.context_len);
        // Only the GUI knows whether this history seed is a fork or a rewind.
    }
    record(runtime, row);
}

pub(crate) fn record_escalation(
    runtime: &AgentRuntime,
    run_id: RunId,
    source_run_id: RunId,
    parent_thread_id: Option<String>,
    config: &RunConfig,
) {
    let mut row = definition(
        runtime,
        event_bus::escalation_thread_id(&run_id.to_string()),
        run_id,
        Role::Orchestrator,
        config,
    );
    row.parent_thread_id = parent_thread_id;
    row.lineage_kind = Some(ThreadLineageKind::Escalation);
    row.escalation_source_run_id = Some(source_run_id.to_string());
    record(runtime, row);
}

fn definition(
    runtime: &AgentRuntime,
    thread_id: String,
    run_id: RunId,
    role: Role,
    config: &RunConfig,
) -> ThreadUpsert {
    let now = crate::run_store::review_time_ns();
    ThreadUpsert {
        thread_id,
        root_run_id: Some(run_id.to_string()),
        chat_role: Some(role.name().into()),
        project_root: runtime.shared.run_project(config).and_then(|project| {
            match project.root.canonicalize() {
                Ok(root) => Some(root.to_string_lossy().into_owned()),
                Err(error) => {
                    // An uncanonical path must not grant project visibility.
                    tracing::warn!(%run_id, %error, "thread project could not be canonicalized");
                    None
                }
            }
        }),
        created_at_ns: now,
        updated_at_ns: now,
        ..ThreadUpsert::default()
    }
}

fn record(runtime: &AgentRuntime, row: ThreadUpsert) {
    if let Some(store) = runtime.shared.run_store.get()
        && let Err(error) = store.handle.upsert_thread_runtime(&row)
    {
        tracing::warn!(thread_id = %row.thread_id, %error, "thread lineage was not saved");
    }
}
