//! Owned graph startup and reason-specific retries. Legacy hosts never call it.
use crate::recovery_startup::*;
use std::{
    collections::BTreeSet,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use workbench_host::bootstrap::{
    core_startup::{
        CoreStartupDiagnostics, CoreStartupFuture, CoreStartupHook, CoreStartupInputs,
        RecoveryAdminPort,
    },
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
pub mod admin;
use admin::{
    RecoveryAdminActor, RecoveryAdminContext, RecoveryAdminHandler, RecoveryRequest,
    UnsupportedRecoveryAdmin,
};

#[derive(Clone)]
pub struct RecoveryDiagnostics {
    report: Arc<RwLock<RecoveryStartupReport>>,
    retry_count: Arc<AtomicUsize>,
    jobs: Arc<AtomicUsize>,
    admin: Arc<RecoveryAdminActor>,
}
impl CoreStartupDiagnostics for RecoveryDiagnostics {
    fn has_pending_work(&self) -> bool {
        self.retry_count.load(Ordering::Acquire) != 0 || self.jobs.load(Ordering::Acquire) != 0
    }
    fn recovery_admin(&self) -> &dyn RecoveryAdminPort {
        self.admin.as_ref()
    }
    fn snapshot(&self) -> serde_json::Value {
        serde_json::to_value(
            &*self
                .report
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
        start_recovery(inputs, services, Arc::new(UnsupportedRecoveryAdmin))
    }
}
/// Installs the leaf implementation without introducing a host-to-core dependency.
pub struct CoreGraphRecoveryWithAdmin(Arc<dyn RecoveryAdminHandler>);
impl CoreGraphRecovery {
    pub fn with_admin_handler(
        handler: Arc<dyn RecoveryAdminHandler>,
    ) -> CoreGraphRecoveryWithAdmin {
        CoreGraphRecoveryWithAdmin(handler)
    }
}
impl CoreStartupHook for CoreGraphRecoveryWithAdmin {
    fn start<'a>(
        &'a self,
        inputs: CoreStartupInputs,
        services: &'a mut BackgroundServices,
    ) -> CoreStartupFuture<'a> {
        start_recovery(inputs, services, self.0.clone())
    }
}
fn start_recovery<'a>(
    inputs: CoreStartupInputs,
    services: &'a mut BackgroundServices,
    handler: Arc<dyn RecoveryAdminHandler>,
) -> CoreStartupFuture<'a> {
    Box::pin(async move {
        match inputs.mode {
            workbench_host::bootstrap::core_startup::CoreBootMode::Cold => {
                if inputs.runtime.has_planned_restart_handoff() {
                    return Err("planned proof in cold bootstrap".into());
                }
                inputs.runtime.quarantine_cold_restarts()?;
                inputs.runtime.quarantine_cold_learning_from_proc()?;
            }
            workbench_host::bootstrap::core_startup::CoreBootMode::PlannedHandoff => {
                if !inputs.runtime.has_planned_restart_handoff() {
                    return Err("planned bootstrap requires inherited proof".into());
                }
            }
        }
        let config = config::Config::load_strict(&config::LoadOptions {
            user_config_dir: Some(inputs.options.config_dir.clone()),
            project_dir: Some(inputs.options.project_root.clone()),
            read_env: false,
            ..Default::default()
        })?;
        let catalog = RecoveryCatalog::preload_for_projects(
            &inputs.options,
            &config,
            &inputs.projects,
            services,
        )
        .await?;
        catalog.publish_projects(&inputs.runtime, &inputs.projects);
        let (admin, mut requests) = RecoveryAdminActor::channel();
        let diagnostics = RecoveryDiagnostics {
            report: Default::default(),
            retry_count: Default::default(),
            jobs: Default::default(),
            admin: Arc::new(admin),
        };
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
                if let Err(reason) = workbench_host::bootstrap::recovery::authority::claim_planned_owner_threads(&inputs.runtime, &inputs.owner) {
                    let _ = ready.send(Err(format!("planned owner adoption: {reason:?}")));
                    return Ok(());
                }
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
                    admin::reconcile(&inputs.runtime).await;
                    let job = admin::JobGuard::start(&output.jobs, &inputs.work_changed);
                    let outcome = attempt(&coordinator, &inputs.runtime, &inputs.host, component);
                    admin::reconcile(&inputs.runtime).await;
                    drop(job);
                    match outcome {
                        AutomaticAttempt::Finished(outcome) => {
                            if outcome.state == RecoveryState::Blocked { retry.insert(component.manifest_id.clone()); }
                            report.components.push(outcome);
                        }
                        AutomaticAttempt::Capacity => { retry.insert(component.manifest_id.clone()); }
                        AutomaticAttempt::Contended => {}
                    }
                }
                output.retry_count.store(retry.len(), Ordering::Release);
                *output.report.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                inputs.work_changed.notify_one();
                tracing::info!(report = %serde_json::to_string(&report)?, "core graph startup recovery");
                let _ = ready.send(Ok(()));
                // External-process death/lease expiry cannot notify our EventBus.
                // Only owner-blocked components enter this owned retry loop.
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.tick().await;
                loop {
                    tokio::select! { biased;
                        _ = &mut stop => break,
                        request = requests.recv() => {
                            let Some(request) = request else { break; };
                            match request {
                                RecoveryRequest::Retry { manifest_id, reply } => {
                                    retry.insert(manifest_id);
                                    output.retry_count.store(retry.len(), Ordering::Release);
                                    let _ = reply.send(Ok(vec![]));
                                }
                                RecoveryRequest::Resume { request, reply } => {
                                    // Admission already owns the graph reservation. It is
                                    // not an automatic retry even if the old display is Blocked.
                                    let was_retry = retry.remove(&request.manifest_id);
                                    output.retry_count.store(retry.len(), Ordering::Release);
                                    let id = request.manifest_id.clone();
                                    let context = RecoveryAdminContext::for_job(&coordinator, &inputs.host);
                                    let result = admin::run_job(context, handler.clone(), request, &output.jobs, &inputs.work_changed).await;
                                    // Unsupported performs no write; preserve the old automatic
                                    // retry when the unregistered leaf returns its reservation.
                                    if was_retry && result == Err(workbench_host::bootstrap::core_startup::RecoveryAdminError::Unsupported) {
                                        retry.insert(id);
                                    }
                                    admin::reconcile(&inputs.runtime).await;
                                    let _ = reply.send(result);
                                }
                            }
                            output.retry_count.store(retry.len(), Ordering::Release);
                            inputs.work_changed.notify_one();
                        }
                        _ = tick.tick(), if !retry.is_empty() => {
                            let Some(_producer) = inputs.runtime.producer_gate().try_enter() else { continue; };
                            let scan = match coordinator.scan() {
                                Ok(scan) => scan,
                                Err(reason) => {
                                    for current in report.components.iter_mut().filter(|c| retry.contains(&c.manifest_id)) {
                                        current.state = RecoveryState::Quarantined;
                                        current.reason = Some(reason);
                                        current.manual_continue = false;
                                    }
                                    *output.report.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report;
                                    output.retry_count.store(0, Ordering::Release);
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
                                admin::reconcile(&inputs.runtime).await;
                                let job = admin::JobGuard::start(&output.jobs, &inputs.work_changed);
                                let outcome = attempt(&coordinator, &inputs.runtime, &inputs.host, &component);
                                admin::reconcile(&inputs.runtime).await;
                                drop(job);
                                match outcome {
                                    AutomaticAttempt::Finished(outcome) => {
                                        if outcome.state != RecoveryState::Blocked { retry.remove(&component.manifest_id); }
                                        if let Some(current) = report.components.iter_mut().find(|c| c.manifest_id == component.manifest_id) { *current = outcome; }
                                        else { report.components.push(outcome); }
                                    }
                                    AutomaticAttempt::Contended => { retry.remove(&component.manifest_id); }
                                    AutomaticAttempt::Capacity => {}
                                }
                            }
                            output.retry_count.store(retry.len(), Ordering::Release);
                            report.scan = scan;
                            *output.report.write().unwrap_or_else(std::sync::PoisonError::into_inner) = report.clone();
                            inputs.work_changed.notify_one();
                            tracing::info!(report = %serde_json::to_string(&report)?, "core graph recovery retry");
                        }
                    }
                }
                output.retry_count.store(0, Ordering::Release);
                inputs.work_changed.notify_one();
                Ok(())
            });
        started
            .await?
            .map_err(|reason| -> workbench_host::bootstrap::BootstrapError { reason.into() })?;
        Ok(Arc::new(diagnostics) as Arc<dyn CoreStartupDiagnostics>)
    })
}

pub(crate) enum AutomaticAttempt {
    Finished(RecoveryDiagnostic),
    Contended,
    Capacity,
}

/// Keep the original graph slot until command binding publication has settled.
/// Unwinding drops the slot only after the publication closure has unwound.
pub(crate) fn publish_activation(
    (runs, reservation): (Vec<runtime::RunId>, runtime::ActivatedReservation),
    publish: impl FnOnce(),
) -> Vec<runtime::RunId> {
    publish();
    reservation.release();
    runs
}

pub(crate) fn attempt(
    coordinator: &StartupCoordinator<'_>,
    runtime: &runtime::AgentRuntime,
    host: &Arc<std::sync::Mutex<workbench_host::CommandHost>>,
    component: &ComponentStatus,
) -> AutomaticAttempt {
    let reservation = if matches!(
        component.disposition,
        Disposition::Prepared | Disposition::AwaitingAuthority
    ) {
        match runtime.reserve_automatic_activation(&component.manifest_id) {
            Ok(reservation) => Some(reservation),
            Err(runtime::RuntimeError::ActivationReservationUnavailable { reason }) => {
                return match reason {
                    runtime::ActivationReservationProblem::Contended => AutomaticAttempt::Contended,
                    runtime::ActivationReservationProblem::Capacity => AutomaticAttempt::Capacity,
                };
            }
            Err(error) => {
                tracing::warn!(%error, manifest_id = %component.manifest_id, "automatic recovery reservation unavailable");
                return AutomaticAttempt::Capacity;
            }
        }
    } else {
        None
    };
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
                .and_then(|prepared| {
                    let reservation =
                        reservation.expect("executable disposition reserved at attempt start");
                    runtime
                        .check_automatic_activation(&reservation)
                        .map_err(|_| StartupBlocked::NeedsAttention)?;
                    let bindings = prepared.command_bindings();
                    let guard = host
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .service()
                        .begin_root_recovery(bindings)
                        .map_err(|_| StartupBlocked::CurrentBindingRequired)?;
                    // No host lock spans activation/DB/runtime work. The guard
                    // also removes the admission fence on error or unwind.
                    let activated = prepared.activate(runtime, reservation)?;
                    Ok(publish_activation(activated, || {
                        host.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .service_mut()
                            .complete_root_recovery(guard);
                    }))
                }) {
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
    AutomaticAttempt::Finished(RecoveryDiagnostic {
        manifest_id: component.manifest_id.clone(),
        runs: component.runs.clone(),
        state,
        reason,
        manual_continue: state == RecoveryState::Released
            || reason == Some(StartupBlocked::AlreadyActivated),
    })
}

#[cfg(test)]
mod tests;
