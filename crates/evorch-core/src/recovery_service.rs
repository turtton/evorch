//! Owned graph startup and reason-specific retries. Legacy hosts never call it.
use crate::recovery_startup::*;
use std::{
    collections::BTreeSet,
    sync::{Arc, RwLock},
    time::Duration,
};
use workbench_host::bootstrap::{
    core_startup::{CoreStartupDiagnostics, CoreStartupFuture, CoreStartupHook, CoreStartupInputs},
    recovery::catalog::RecoveryCatalog,
    services::BackgroundServices,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    Activated,
    Blocked,
    Released,
    Quarantined,
    Skipped,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RecoveryDiagnostic {
    pub manifest_id: String,
    pub runs: Vec<runtime::RunId>,
    pub state: RecoveryState,
    pub reason: Option<StartupBlocked>,
    pub manual_continue: bool,
}
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RecoveryStartupReport {
    pub scan: StartupScan,
    pub components: Vec<RecoveryDiagnostic>,
}
#[derive(Clone, Default)]
pub struct RecoveryDiagnostics(Arc<RwLock<RecoveryStartupReport>>);
impl CoreStartupDiagnostics for RecoveryDiagnostics {
    fn has_pending_work(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .components
            .iter()
            .any(|component| component.state == RecoveryState::Blocked)
    }
    fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(
            &*self
                .0
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .expect("payload-free report is serializable")
    }
}
/// Core startup implementation; embedded open remains unchanged.
pub struct CoreGraphRecovery;
impl CoreStartupHook for CoreGraphRecovery {
    fn start<'a>(
        &'a self,
        inputs: CoreStartupInputs,
        services: &'a mut BackgroundServices,
    ) -> CoreStartupFuture<'a> {
        Box::pin(async move {
            let config = config::Config::load_strict(&config::LoadOptions {
                user_config_dir: Some(inputs.options.config_dir.clone()),
                project_dir: Some(inputs.options.project_root.clone()),
                read_env: false,
                ..Default::default()
            })?;
            let catalog = RecoveryCatalog::preload(&inputs.options, &config, services).await?;
            catalog.publish_projects(&inputs.runtime, &inputs.projects);
            let diagnostics = RecoveryDiagnostics::default();
            let output = diagnostics.clone();
            let (ready, started) = tokio::sync::oneshot::channel();
            services.start("graph-recovery", |mut stop| async move {
                let coordinator = match StartupCoordinator::new(StartupInputs {
                    runtime: &inputs.runtime, owner: &inputs.owner,
                    storage: &inputs.storage, writer: &inputs.writer,
                    options: &inputs.options, projects: &inputs.projects,
                }, inputs.startup, catalog) {
                    Ok(coordinator) => coordinator,
                    Err(reason) => {
                        let _ = ready.send(Err(format!("graph startup fence: {reason:?}")));
                        return Ok(());
                    }
                };
                let scan = match coordinator.scan() {
                    Ok(scan) => scan,
                    Err(reason) => {
                        let _ = ready.send(Err(format!("graph startup scan: {reason:?}")));
                        return Ok(());
                    }
                };
                let mut report = RecoveryStartupReport { scan, components: vec![] };
                let mut retry = BTreeSet::new();
                for component in &report.scan.components {
                    let outcome = attempt(&coordinator, &inputs.runtime, component);
                    if outcome.state == RecoveryState::Blocked { retry.insert(component.manifest_id.clone()); }
                    report.components.push(outcome);
                }
                *output.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                inputs.work_changed.notify_one();
                tracing::info!(report = %serde_json::to_string(&report)?, "core graph startup recovery");
                let _ = ready.send(Ok(()));
                // External-process death/lease expiry cannot notify our EventBus.
                // Only owner-blocked components enter this owned retry loop.
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.tick().await;
                while !retry.is_empty() {
                    tokio::select! { biased;
                        _ = &mut stop => break,
                        _ = tick.tick() => {
                            let Some(_producer) = inputs.runtime.producer_gate().try_enter() else { continue; };
                            let scan = match coordinator.scan() {
                                Ok(scan) => scan,
                                Err(reason) => {
                                    for current in report.components.iter_mut().filter(|c| retry.contains(&c.manifest_id)) {
                                        current.state = RecoveryState::Quarantined;
                                        current.reason = Some(reason);
                                        current.manual_continue = false;
                                    }
                                    *output.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report;
                                    inputs.work_changed.notify_one();
                                    return Err(format!("graph retry scan: {reason:?}").into());
                                }
                            };
                            // Retention or an external writer may remove a component
                            // while it is owner-blocked. Do not retain a phantom retry
                            // or imply that its old receipt is still executable.
                            for id in retry.iter().filter(|id| !scan.components.iter().any(|c| c.manifest_id == **id)).cloned().collect::<Vec<_>>() {
                                retry.remove(&id);
                                if let Some(current) = report.components.iter_mut().find(|c| c.manifest_id == id) {
                                    current.state = RecoveryState::Quarantined;
                                    current.reason = Some(StartupBlocked::NeedsAttention);
                                    current.manual_continue = false;
                                }
                            }
                            for component in scan.components.iter().filter(|c| retry.contains(&c.manifest_id)).cloned().collect::<Vec<_>>() {
                                let outcome = attempt(&coordinator, &inputs.runtime, &component);
                                if outcome.state != RecoveryState::Blocked { retry.remove(&component.manifest_id); }
                                if let Some(current) = report.components.iter_mut().find(|c| c.manifest_id == component.manifest_id) { *current = outcome; }
                            }
                            report.scan = scan;
                            *output.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                            inputs.work_changed.notify_one();
                            tracing::info!(report = %serde_json::to_string(&report)?, "core graph recovery retry");
                        }
                    }
                }
                Ok(())
            });
            started
                .await?
                .map_err(|reason| -> workbench_host::bootstrap::BootstrapError { reason.into() })?;
            Ok(Arc::new(diagnostics) as Arc<dyn CoreStartupDiagnostics>)
        })
    }
}
fn attempt(
    coordinator: &StartupCoordinator<'_>,
    runtime: &runtime::AgentRuntime,
    component: &ComponentStatus,
) -> RecoveryDiagnostic {
    let (state, reason) = match component.disposition {
        Disposition::Released => (RecoveryState::Released, Some(StartupBlocked::Released)),
        Disposition::AlreadyActivated => (
            RecoveryState::Quarantined,
            Some(StartupBlocked::AlreadyActivated),
        ),
        Disposition::Superseded => (RecoveryState::Skipped, None),
        Disposition::NeedsAttention => (
            RecoveryState::Quarantined,
            Some(StartupBlocked::NeedsAttention),
        ),
        Disposition::Prepared | Disposition::AwaitingAuthority => {
            match coordinator
                .prepare(&component.manifest_id)
                .and_then(|prepared| prepared.activate(runtime))
            {
                Ok(_) => (RecoveryState::Activated, None),
                Err(StartupBlocked::OwnerBlocked) => {
                    (RecoveryState::Blocked, Some(StartupBlocked::OwnerBlocked))
                }
                // Missing metadata must hold even Prepared claims: no adoption,
                // activation, provider admission, or conditional release.
                Err(StartupBlocked::CatalogUnavailable) => (
                    RecoveryState::Quarantined,
                    Some(StartupBlocked::NeedsAttention),
                ),
                Err(reason) => match coordinator.decline(&component.manifest_id) {
                    Ok(()) => (RecoveryState::Released, Some(reason)),
                    Err(StartupBlocked::OwnerBlocked) => {
                        (RecoveryState::Blocked, Some(StartupBlocked::OwnerBlocked))
                    }
                    Err(_) => {
                        let reason = if coordinator
                            .cleanup_activated(&component.manifest_id)
                            .is_ok()
                        {
                            StartupBlocked::AlreadyActivated
                        } else {
                            reason
                        };
                        (RecoveryState::Quarantined, Some(reason))
                    }
                },
            }
        }
    };
    RecoveryDiagnostic {
        manifest_id: component.manifest_id.clone(),
        runs: component.runs.clone(),
        state,
        reason,
        manual_continue: state == RecoveryState::Released
            || reason == Some(StartupBlocked::AlreadyActivated),
    }
}
