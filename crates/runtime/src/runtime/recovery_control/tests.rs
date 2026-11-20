use super::*;
use crate::RunStore;

struct NoProvider;
#[async_trait::async_trait]
impl AgentModel for NoProvider {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "unused".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("readback must not request a provider")
    }
}
fn runtime() -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(NoProvider),
    )
}
fn claim(manifest: &str, run: &str) -> storage::recovery::RecoveryClaim {
    storage::recovery::RecoveryClaim {
        claim_id: format!("claim-{manifest}"),
        manifest_id: manifest.into(),
        state: storage::recovery::RecoveryClaimState::Prepared,
        members: vec![storage::recovery::RecoveryClaimMember {
            run_id: run.into(),
            old_incarnation: "old".into(),
            reserved_revision: 0,
            new_incarnation: "new".into(),
        }],
    }
}
fn manual(rt: &AgentRuntime, manifest: &str) -> ActivationReservation {
    rt.reserve_manual_activation(
        manifest,
        GraphEffectKey {
            operation_id: format!("op-{manifest}"),
            attempt_nonce: "nonce".into(),
        },
        0,
    )
    .unwrap()
    .into()
}
#[test]
fn manual_readback_retains_unknown_and_cannot_release_automatic_or_start_effects() {
    let root = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: root.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    writer
        .save_resume_intent(
            storage::resume_intent::ResumeIntentRecord {
                run_id: "run-1".into(),
                incarnation: "old".into(),
                payload: "{}".into(),
            },
            None,
        )
        .unwrap();
    let rt = runtime().with_run_store(RunStore::open(&config, writer).unwrap());
    rt.supervise_activation(manual(&rt, "manual"), claim("manual", "run-1"));
    rt.supervise_activation(manual(&rt, "unknown"), claim("unknown", "missing"));
    let automatic = rt.reserve_automatic_activation("automatic").unwrap();
    rt.supervise_activation(automatic.into(), claim("automatic", "run-1"));
    assert_eq!(rt.reconcile_manual_activation_reservations().unwrap(), 1);
    assert!(rt.reserve_automatic_activation("automatic").is_err());
    assert!(rt.reserve_automatic_activation("unknown").is_err());
    assert!(rt.reserve_automatic_activation("manual").is_ok());
    assert_eq!(rt.reconcile_manual_activation_reservations().unwrap(), 0);
    assert_eq!(rt.graph_effect_counts(), [0, 0, 0]);
    assert!(rt.list_agents().is_empty());
    assert_eq!(rt.reconcile_automatic_activation_reservations().unwrap(), 1);
    assert!(rt.reserve_automatic_activation("automatic").is_ok());
    assert!(rt.reserve_automatic_activation("unknown").is_err());
    let store = rt.shared.run_store.get().unwrap();
    assert_eq!(
        store.with_database(|db| db.resume_intents()).unwrap()[0].incarnation,
        "old"
    );
}
#[test]
fn missing_store_does_not_release_the_original_manual_token() {
    let rt = runtime();
    rt.supervise_activation(manual(&rt, "unknown"), claim("unknown", "missing"));
    assert_eq!(rt.reconcile_manual_activation_reservations().unwrap(), 0);
    assert!(rt.reserve_automatic_activation("unknown").is_err());
    assert_eq!(rt.graph_effect_counts(), [0, 0, 0]);
}
