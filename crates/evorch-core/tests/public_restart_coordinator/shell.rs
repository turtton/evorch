use super::{fixture::*, result};
use runtime::AgentModel;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use workbench_host::admin::*;
use workbench_protocol::{
    namespaces::lifecycle::{restart::*, *},
    wire::{InstanceEpoch, Revision},
};

#[derive(Default)]
struct ShellModel {
    input: model::Model,
    issued: AtomicBool,
    tool_ok: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    released: AtomicBool,
}
impl ShellModel {
    fn release_all(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}
#[async_trait::async_trait]
impl AgentModel for ShellModel {
    fn selected_model(&self, role: runtime::Role, hint: Option<&str>) -> String {
        self.input.selected_model(role, hint)
    }
    fn requires_admission(&self) -> bool {
        false
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<runtime::quiesce::ProviderInput> {
        self.input.checkpoint_provider_input(run)
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
    ) -> Result<(), runtime::RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: runtime::Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let mut response = self
            .input
            .complete(invocation, role, messages, tools)
            .await?;
        if !self.issued.swap(true, Ordering::SeqCst) {
            response.message.content = vec![providers::ContentBlock::ToolUse {
                id: "blocker-call".into(),
                name: "shell".into(),
                input: serde_json::json!({"command":"read value","yield_ms":0}),
            }];
            response.finish_reason = providers::FinishReason::ToolUse;
        } else {
            self.tool_ok.store(messages.last().is_some_and(|m| m.content.iter().any(|b| matches!(b, providers::ContentBlock::ToolResult { tool_call_id, is_error: false, .. } if tool_call_id == "blocker-call"))), Ordering::SeqCst);
            self.entered.notify_one();
            loop {
                let released = self.release.notified();
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                released.await;
            }
        }
        Ok(response)
    }
}
async fn fixture() -> (Fixture, Arc<ShellModel>) {
    let model = Arc::new(ShellModel::default());
    let f = Fixture::with_model(model.clone()).await;
    let run = f
        .host
        .runtime
        .delegate_chat(
            "thread",
            runtime::Role::Worker,
            "start shell".into(),
            runtime::RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    model.entered.notified().await;
    assert!(
        model.tool_ok.load(Ordering::SeqCst),
        "real shell tool must have succeeded"
    );
    f.recipes.0.lock().unwrap().insert(
        run,
        runtime::quiesce::RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
            policy: runtime::quiesce::ResumePolicy::CoreOwned,
            goal_id: None,
            team: None,
            lane: None,
            descriptor: None,
        },
    );
    let request = f.request("request").await;
    result(&f, request).await.unwrap();
    (f, model)
}
async fn binding(f: &Fixture) -> InterruptBinding {
    let mut changes = f.core.subscribe();
    loop {
        assert!(
            !matches!(
                f.core.snapshot().phase,
                evorch_core::restart::RestartPhase::FailedClosed
                    | evorch_core::restart::RestartPhase::Running
            ),
            "restart ended before shell binding"
        );
        let LifecycleReply::Restart(RestartReply::ShellBlockers(blockers)) = f
            .leaf
            .query(
                &LifecycleQuery::Restart(RestartQuery::ShellBlockers),
                &f.connection.verified,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        if let Some(blocker) = blockers.blockers.first() {
            assert!(
                !blocker.teardown_confirmed,
                "Wait must not terminate the real shell"
            );
            assert!(!blocker.observed, "Wait must not acknowledge its result");
            return blocker.binding.clone();
        }
        changes.changed().await.unwrap();
    }
}
#[tokio::test]
async fn shell_deadline_retains_run_thread_and_operation_after_reopen() {
    use evorch_core::restart::{PreparationFailure, PreparationStage, RestartPhase};
    let (f, model) = fixture().await;
    let _ = binding(&f).await;
    let snapshot = f.core.snapshot();
    let attempt = snapshot.id.unwrap();
    let run = snapshot.blockers.first().unwrap().run;
    tokio::time::pause();
    // The configured preparation limit is 300s. The held model request cannot
    // finish until the test explicitly releases it; use only virtual time.
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    assert!(f.core.wait_sealed(attempt).await.is_err());
    assert_eq!(f.core.snapshot().phase, RestartPhase::Running);
    assert!(f.core.snapshot().blockers.is_empty());
    let recorded = f.core.operation_diagnostic(&id("request")).unwrap();
    assert_eq!(
        Some(recorded.clone()),
        f.core.preparation_diagnostic(attempt)
    );
    assert_eq!(recorded.stage, PreparationStage::SafePoints);
    assert_eq!(recorded.reason, PreparationFailure::Deadline);
    assert_eq!((recorded.total, recorded.omitted), (1, 0));
    let cause = recorded.causes.first().unwrap();
    assert_eq!(cause.run, run);
    assert_eq!(cause.thread.as_deref(), Some("thread"));
    assert!(cause.blocker.is_some());
    assert!(recorded.retryable);
    tokio::time::resume();
    model.release_all();
    f.shutdown().await;
}

fn intent(binding: InterruptBinding, op: &str, confirmed: bool) -> AdminIntent {
    LifecycleCommand::Restart(RestartCommand::Interrupt(InterruptShell {
        binding,
        operation_id: id(op),
        confirmed,
    }))
}
async fn refused(f: &Fixture, intent: AdminIntent) {
    match f.submit(intent) {
        Err(_) => {}
        Ok(rx) => assert!(rx.await.unwrap().is_err()),
    }
}
#[tokio::test]
async fn every_interrupt_binding_component_has_a_negative_and_wait_never_signals() {
    let (mut f, model) = fixture().await;
    let original = binding(&f).await;
    let before = f.core.snapshot();

    refused(&f, intent(original.clone(), "unconfirmed", false)).await;
    let mut variants = vec![];
    let mut b = original.clone();
    b.restart_id = id("other-restart");
    variants.push(b);
    let mut b = original.clone();
    b.core_epoch = InstanceEpoch(999);
    variants.push(b);
    let mut b = original.clone();
    b.connection_epoch = InstanceEpoch(999);
    variants.push(b);
    let mut b = original.clone();
    b.run_incarnation = id("other-incarnation");
    variants.push(b);
    let mut b = original.clone();
    b.blocker_identity = id("other-job");
    variants.push(b);
    let mut b = original.clone();
    b.revision = Revision(999);
    variants.push(b);
    let mut b = original.clone();
    b.phase = if b.phase == RestartPhase::PreparingDrains {
        RestartPhase::WaitingSafePoint
    } else {
        RestartPhase::PreparingDrains
    };
    variants.push(b);
    for (n, b) in variants.into_iter().enumerate() {
        refused(&f, intent(b, &format!("negative-{n}"), true)).await;
    }
    let another = Connection::new();
    let response = f
        .submit_on(&another, intent(original.clone(), "other-connection", true))
        .unwrap();
    assert!(response.await.unwrap().is_err());
    let after = f.core.snapshot();
    assert_eq!(before.id, after.id);
    assert_eq!(before.revision, after.revision);
    assert_eq!(
        binding(&f).await,
        original,
        "Wait neither signals nor acknowledges the original job"
    );
    // Positive control uses the same real job: success is terminal reap, not ACK.
    f.submit(intent(original, "positive", true))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    // Writer completion is the event proving the audited continuation finished.
    f.event(4, "positive").await;
    let record = storage::Database::open(&f.host.composition.storage)
        .unwrap()
        .admin_audit_record("positive")
        .unwrap()
        .unwrap();
    assert_eq!(
        record.outcome,
        Some(storage::admin_audit::AuditOutcome::Succeeded)
    );
    model.release_all();
    f.shutdown().await;
}
#[tokio::test]
async fn interrupt_refuses_revoked_connection_while_audit_start_ack_is_held() {
    let (mut f, model) = fixture().await;
    let original = binding(&f).await;
    let expected_job = original.blocker_identity.clone();
    let mut selected = Connection::new();
    let mut b = original;
    b.connection_epoch = selected.verified.epoch();
    f.port.hold.store(2, Ordering::SeqCst);
    let response = f.submit_on(&selected, intent(b, "revoked", true)).unwrap();
    f.event(2, "revoked").await;
    selected.lease.take();
    f.port.release();
    assert!(response.await.unwrap().is_err());
    assert_eq!(binding(&f).await.blocker_identity, expected_job);
    model.release_all();
    f.shutdown().await;
}

#[tokio::test]
async fn interrupt_audit_failures_do_not_signal_and_completion_failure_never_repeats() {
    let (mut f, model) = fixture().await;
    let selected = binding(&f).await;
    for stage in [1, 2] {
        f.port.fail.store(stage, Ordering::SeqCst);
        refused(
            &f,
            intent(selected.clone(), &format!("audit-{stage}"), true),
        )
        .await;
        assert_eq!(binding(&f).await, selected);
    }
    f.port.fail.store(4, Ordering::SeqCst);
    let original = intent(selected, "result-unknown", true);
    f.submit(original.clone()).unwrap().await.unwrap().unwrap();
    f.event(4, "result-unknown").await;
    // The injected failure event fires inside the audit write, before the task
    // finishes finalization and leaves the active set. Readback reports
    // Started/Pending while the operation is still live, so wait for its end.
    f.service.wait_operation_inactive("result-unknown").await;
    let row = storage::Database::open(&f.host.composition.storage)
        .unwrap()
        .admin_audit_record("result-unknown")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, storage::admin_audit::AuditState::Started);
    assert_eq!(row.outcome, None);
    // The handle is no longer interruptible; success here therefore proves
    // readback of the same started operation, not a second effect/inspection.
    let readback = f
        .submit(original)
        .unwrap()
        .await
        .unwrap()
        .unwrap()
        .into_receipt();
    assert_eq!(readback.outcome, AdminOutcome::Unknown);
    f.port.fail.store(0, Ordering::SeqCst);
    model.release_all();
    f.shutdown().await;
}

#[tokio::test]
async fn cancel_overtakes_held_interrupt_audit_and_cannot_signal_old_attempt() {
    let (mut f, model) = fixture().await;
    let selected = binding(&f).await;
    f.port.hold.store(2, Ordering::SeqCst);
    let pending = f
        .submit(intent(selected.clone(), "stale-after-audit", true))
        .unwrap();
    f.event(2, "stale-after-audit").await;
    f.submit(f.cancel("cancel-held-interrupt"))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    f.port.release();
    assert!(pending.await.unwrap().is_err());
    result(&f, f.request("next-attempt").await).await.unwrap();
    assert_eq!(
        binding(&f).await.blocker_identity,
        selected.blocker_identity
    );
    model.release_all();
    f.shutdown().await;
}
