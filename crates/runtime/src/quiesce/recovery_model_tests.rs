use super::*;
use crate::{AgentInvocationContext, AgentModel, RuntimeError};
use providers::{ChatResponse, Message, ToolSpec};
use std::sync::atomic::AtomicUsize;

#[derive(Default)]
struct ProjectModel {
    catalogs: AtomicUsize,
    checks: AtomicUsize,
    adopts: AtomicUsize,
    admissions: AtomicUsize,
    requests: AtomicUsize,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for ProjectModel {
    fn publish_model_catalog(&self, _: Arc<catalog::ModelCatalog>) {
        self.catalogs.fetch_add(1, Ordering::SeqCst);
    }
    fn validate_checkpoint_compatibility(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        pins: &[crate::RoutePinRecord],
        input: Option<&crate::quiesce::ProviderInput>,
    ) -> Result<(), RuntimeError> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        if self.catalogs.load(Ordering::SeqCst) == 0 || !pins.is_empty() || input.is_some() {
            return Err(RuntimeError::Model {
                reason: "project catalog/evidence unavailable".into(),
            });
        }
        Ok(())
    }
    fn adopt_recovery_checkpoint(
        &self,
        _: &str,
        _: &[crate::RoutePinRecord],
        _: Option<&crate::quiesce::ProviderInput>,
    ) {
        self.adopts.fetch_add(1, Ordering::SeqCst);
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "project".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
}
async fn publish(models: &crate::RecoveryModels, run: RunId) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("models-dev.json"),
        r#"{"fetched_at":0,"api":{}}"#,
    )
    .unwrap();
    // Holding the refresh lock makes even stale cache reads deterministic and
    // physically prevents this fixture from scheduling a network refresh.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.path().join("models-dev.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let catalog = catalog::ModelCatalog::load_or_refresh(dir.path())
        .await
        .unwrap();
    models
        .model(run)
        .unwrap()
        .publish_model_catalog(Arc::new(catalog));
}
async fn project_fixture() -> (Fixture, RunId, std::path::PathBuf) {
    let f = Fixture::new(0);
    let project = f._dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    f.runtime.set_project_root(project.clone()).unwrap();
    let run = saved(&f).await;
    (f, run, project)
}
fn project_runtime(f: &Fixture, wrapper: crate::compose::SwitchableModel) -> AgentRuntime {
    let model: Arc<dyn AgentModel> = Arc::new(wrapper);
    new_process(f).with_project_models(Arc::new(move |_| Some(Arc::clone(&model))))
}
fn project_authority(
    f: &Fixture,
    run: RunId,
    project: std::path::PathBuf,
) -> (crate::ownership::OwnerHost, CurrentRecoveryAuthority) {
    let (owner, mut current) = authority(f, run);
    current.members.get_mut(&run).unwrap().project_root = Some(project);
    (owner, current)
}

#[tokio::test]
async fn project_catalog_validation_adoption_admission_and_execution_share_one_pin_after_ack_reload()
 {
    let (f, run, project) = project_fixture().await;
    let selected = Arc::new(ProjectModel::default());
    let replacement = Arc::new(ProjectModel::default());
    let wrapper = crate::compose::SwitchableModel::new(selected.clone());
    let runtime = project_runtime(&f, wrapper.clone());
    let mut paused = prepare(&f, &runtime);
    let (_owner, authority) = project_authority(&f, run, project);
    let models = runtime.pin_recovery_models(&authority);
    publish(&models, run).await;
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let reload = replacement.clone();
    runtime.on_execution(
        crate::runtime::HookPoint::RecoveryActivated,
        &runtime.execution_snapshot(run).unwrap(),
        move || wrapper.replace(reload),
    );
    runtime
        .activate_component_with_models(paused, authority, models)
        .unwrap();
    selected.entered.notified().await;
    assert_eq!(selected.catalogs.load(Ordering::SeqCst), 1);
    assert_eq!(selected.checks.load(Ordering::SeqCst), 1);
    assert_eq!(selected.adopts.load(Ordering::SeqCst), 1);
    assert_eq!(selected.admissions.load(Ordering::SeqCst), 1);
    assert_eq!(selected.requests.load(Ordering::SeqCst), 1);
    assert_eq!(replacement.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.requests.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    runtime.cancel(run).unwrap();
    let _ = runtime.wait(run).await;
}

#[tokio::test]
async fn inner_reload_at_the_last_pre_ack_boundary_rejects_without_writes_or_requests() {
    let (f, run, project) = project_fixture().await;
    let selected = Arc::new(ProjectModel::default());
    let replacement = Arc::new(ProjectModel::default());
    let wrapper = crate::compose::SwitchableModel::new(selected.clone());
    let runtime = project_runtime(&f, wrapper.clone());
    let mut paused = prepare(&f, &runtime);
    let id = paused.claim().claim_id.clone();
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    let (_owner, authority) = project_authority(&f, run, project);
    let models = runtime.pin_recovery_models(&authority);
    publish(&models, run).await;
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let reload = replacement.clone();
    runtime.on_execution(
        crate::runtime::HookPoint::RecoveryBeforeActivation,
        &runtime.execution_snapshot(run).unwrap(),
        move || wrapper.replace(reload),
    );
    assert!(
        runtime
            .activate_component_with_models(paused, authority, models)
            .is_err()
    );
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert_eq!(selected.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(selected.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.requests.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_retained_context_cannot_cross_an_activation_retry_with_inner_reload() {
    let (f, run, project) = project_fixture().await;
    let selected = Arc::new(ProjectModel::default());
    let replacement = Arc::new(ProjectModel::default());
    let wrapper = crate::compose::SwitchableModel::new(selected.clone());
    let runtime = project_runtime(&f, wrapper.clone());
    let paused = prepare(&f, &runtime);
    let id = paused.claim().claim_id.clone();
    let (_owner, authority) = project_authority(&f, run, project);
    let old_models = runtime.pin_recovery_models(&authority);
    publish(&old_models, run).await;
    // Missing relations consumes the handle but never activates the claim.
    assert!(
        runtime
            .activate_component(
                paused,
                CurrentRecoveryAuthority {
                    members: authority.members.clone()
                }
            )
            .is_err()
    );
    wrapper.replace(replacement.clone());
    publish(&runtime.pin_recovery_models(&authority), run).await;
    let (_proof, startup, root) = evidence(&f, &id);
    let crate::RecoveryClaimHandle::Retryable(mut retry) = runtime
        .recover_recovery_claim(&id, startup, &root, &authority)
        .unwrap()
    else {
        panic!("fully valid Prepared retry expected")
    };
    runtime
        .restore_relations_and_continuations(&mut retry)
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    assert!(
        runtime
            .activate_component_with_models(*retry, authority, old_models)
            .is_err()
    );
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert_eq!(selected.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn missing_project_catalog_never_falls_back_to_the_main_model() {
    let (f, run, project) = project_fixture().await;
    let selected = Arc::new(ProjectModel::default());
    let wrapper = crate::compose::SwitchableModel::new(selected.clone());
    let runtime = project_runtime(&f, wrapper);
    let mut paused = prepare(&f, &runtime);
    let id = paused.claim().claim_id.clone();
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    let (_owner, authority) = project_authority(&f, run, project);
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    assert!(runtime.activate_component(paused, authority).is_err());
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert_eq!(selected.checks.load(Ordering::SeqCst), 1);
    assert_eq!(selected.adopts.load(Ordering::SeqCst), 0);
    assert_eq!(selected.admissions.load(Ordering::SeqCst), 0);
    assert_eq!(selected.requests.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}
