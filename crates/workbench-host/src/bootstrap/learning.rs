//! PR03b: shared legacy-GUI/core learning and passive evidence factories.
//!
//! Run jobs are local and fenced by ADR0024; claim_learning is the durable
//! root/goal idempotency barrier. Every local bus feeds passive diagnostics;
//! atomic database observation receipts deduplicate shared events. A short flock
//! serializes spool intake without a lifetime-elected consumer.
use super::{BootstrapError, services::BackgroundServices};
use runtime::{AgentRuntime, ownership::OwnerHost};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn draft_dir(config: &config::Config, database: &Path) -> Option<PathBuf> {
    config.self_improvement.enabled.then(|| {
        runtime::self_improvement::ImprovementPolicy::from_config(
            &config.self_improvement,
            database
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("")),
        )
        .draft_dir
        .unwrap()
    })
}

pub fn configure(
    runtime: AgentRuntime,
    config: &config::Config,
    storage: storage::StorageConfig,
    writer: storage::StorageHandle,
    root: &Path,
) -> AgentRuntime {
    let runtime = runtime.with_project_slugs(Arc::new(runtime::repository::derive_repo_slug));
    let quick = config
        .agents
        .binding_for("worker", Some("quick"))
        .ok()
        .and_then(|binding| {
            let head = binding.models.into_iter().next()?;
            let route = config.routing.routes.get(&head.route)?.first()?;
            Some(runtime::ModelPreference {
                profile: route.profile.clone(),
                model: route.model.clone(),
                reasoning_effort: head.reasoning_effort,
            })
        });
    let runtime = match quick {
        Some(quick) => runtime.with_learning(runtime::memory_queue::LearningSettings {
            writer: writer.clone(),
            storage: storage.clone(),
            project: runtime::repository::derive_repo_slug(root),
            quick,
        }),
        None => runtime,
    };
    if config.self_improvement.enabled {
        runtime.with_self_improvement(runtime::self_improvement::ImprovementSettings {
            writer,
            project: runtime::repository::derive_repo_slug(root),
            policy: runtime::self_improvement::ImprovementPolicy::from_config(
                &config.self_improvement,
                storage.db_path.parent().unwrap_or(Path::new("")),
            ),
        })
    } else {
        runtime
    }
}

/// Synchronous subscription makes startup deterministic; no process is a permanent collector.
/// Even a later factory failure is drained by the caller's startup guard.
pub fn start(
    services: &mut BackgroundServices,
    runtime: &AgentRuntime,
    owner: Arc<OwnerHost>,
    config: &config::Config,
    storage: &storage::StorageConfig,
    evaluations: eval::evaluator::EvaluationService,
) -> Result<(), BootstrapError> {
    let Some(drafts) = draft_dir(config, &storage.db_path) else {
        let runtime = runtime.clone();
        services.start("learning", |stop| async move {
            let _ = stop.await;
            runtime.drain_learning().await;
            Ok(())
        });
        return Ok(());
    };
    let spool = drafts.join("crash-spool");
    std::fs::create_dir_all(&spool)?;
    let hook = runtime::self_improvement::register_crash_spool(spool.canonicalize()?);
    ingest_available(runtime, &storage.db_path)?;
    let settings = runtime
        .self_improvement_settings()
        .ok_or("self-improvement not configured")?;
    let receiver = runtime.event_bus().subscribe();
    let weak = runtime.downgrade();
    let draining = runtime.clone();
    let intake_db = storage.db_path.clone();
    let drain_db = intake_db.clone();
    let producers = runtime.producer_gate();
    services.start("self-improvement", |stop| async move {
        // The hook outlives producers and the final writer acknowledgement.
        let _hook = hook;
        runtime::self_improvement::ImprovementCollector::new(settings)
            .run_until_gated(
                receiver,
                async move {
                    let _ = stop.await;
                    draining.drain_learning().await;
                    evaluations.wait_drained().await;
                    if let Err(error) = ingest_available(&draining, &drain_db) {
                        tracing::warn!(%error, "crash spool final intake failed");
                    }
                },
                move |event| {
                    if !matches!(
                        &event.kind,
                        event_bus::EventKind::Diagnostic(_)
                            | event_bus::EventKind::Fault(
                                event_bus::FaultEvent::SkillDiagnostic { .. }
                            )
                            | event_bus::EventKind::Lifecycle(
                                event_bus::LifecycleEvent::AgentRunStateChanged {
                                    to: event_bus::AgentRunPhase::Error,
                                    ..
                                }
                            )
                    ) {
                        return false;
                    }
                    let (run, thread) = match &event.kind {
                        event_bus::EventKind::Diagnostic(d) => {
                            (d.run_id.as_deref(), d.thread_id.as_deref())
                        }
                        event_bus::EventKind::Lifecycle(
                            event_bus::LifecycleEvent::AgentRunStateChanged { run_id, .. },
                        ) => (Some(run_id.as_str()), None),
                        _ => (None, None),
                    };
                    if let Some(thread) = thread {
                        // Quiescing still owns the generation until the final drain.
                        let owned = owner.attach(thread).is_ok_and(|o| {
                            o.lease.owner_id == owner.owner_id()
                                && !matches!(o.state, runtime::ownership::OwnerState::Released)
                        });
                        if !owned {
                            return false;
                        }
                        if run.is_none() {
                            return true;
                        }
                    }
                    if let Some(run) = run {
                        return run
                            .parse()
                            .ok()
                            .zip(weak.upgrade())
                            .is_some_and(|(run, runtime)| runtime.owns_background_run(run));
                    }
                    if let Some(runtime) = weak.upgrade()
                        && let Err(error) = ingest_available(&runtime, &intake_db)
                    {
                        tracing::warn!(%error, "crash spool intake failed");
                    }
                    // Local buses need no elected observer: storage receipts fence
                    // duplicate delivery of the same event across owners.
                    true
                },
                producers,
            )
            .await?;
        Ok(())
    });
    Ok(())
}

fn ingest_available(runtime: &AgentRuntime, database: &Path) -> Result<(), storage::StorageError> {
    // Only the read/delete intake is exclusive. No live process pins this lease,
    // so any remaining host can take over on input or final drain without polling.
    if let Some(_lease) = storage::improvement_lease(database)? {
        runtime.ingest_spooled_crashes();
    }
    Ok(())
}
