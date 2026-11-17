use super::*;
use storage::plan_write_gate::{PlanWriteGate, PlanWritePoint, PlanWriteResolution};

/// Release the writer on assertion failure too, so scope unwinding can join it.
struct HeldWriter(Option<Arc<PlanWriteGate>>);
impl HeldWriter {
    fn release(&mut self, resolution: PlanWriteResolution) {
        self.0.take().unwrap().resolve(resolution);
    }
}
impl Drop for HeldWriter {
    fn drop(&mut self) {
        if let Some(gate) = self.0.take() {
            gate.resolve(PlanWriteResolution::Continue);
        }
    }
}
fn plan_events(receiver: &mut event_bus::EventReceiver) -> Vec<event_bus::Event> {
    receiver
        .drain_pending_snapshot()
        .into_iter()
        .filter(|event| matches!(event.kind, event_bus::EventKind::ConversationPlan(_)))
        .collect()
}

#[tokio::test]
async fn real_writer_wait_releases_goal_and_runs_for_updates_and_reconciliation() {
    for reconcile in [false, true] {
        let f = Fixture::new();
        let run = f.start("thread").await;
        let other = f.start("other").await;
        if reconcile {
            f.add(run);
        }
        let goal = f
            .runtime
            .create_thread_goal("other", other, "objective".into(), vec!["criterion".into()])
            .unwrap();
        let gate = PlanWriteGate::new();
        f.storage
            .handle()
            .install_plan_write_gate(gate.clone())
            .unwrap();
        gate.arm(PlanWritePoint::BeforeCommit);
        let mut events = f.runtime.event_bus().subscribe();
        std::thread::scope(|scope| {
            let update = scope.spawn(|| {
                let mailbox = f.mailbox(run);
                let caller = f.caller(run, &mailbox);
                if reconcile {
                    assert!(f.runtime.reconcile_plan_or_report(&caller));
                } else {
                    tools::update(&f.runtime, &caller, addition(0, "pending")).unwrap();
                }
            });
            let mut held = HeldWriter(Some(gate.clone()));
            assert_eq!(gate.wait_reached(), PlanWritePoint::BeforeCommit);
            // This signal is emitted by the real writer inside the uncommitted
            // transaction, not a runtime hook placed before its DB call.
            assert!(f.runtime.shared.thread_goals.try_lock().is_ok());
            assert!(f.runtime.shared.runs.try_lock().is_ok());
            assert!(
                f.runtime
                    .shared
                    .plan_authority
                    .get("thread")
                    .try_lock()
                    .is_err()
            );
            assert!(
                f.runtime
                    .shared
                    .plan_authority
                    .get("other")
                    .try_lock()
                    .is_ok()
            );
            assert_eq!(
                f.runtime.inspect_agent(other).unwrap().phase,
                AgentRunPhase::Running
            );
            f.runtime
                .set_goal_checks_paused("other", &goal.goal_id, true)
                .unwrap();
            assert!(f.runtime.thread_goal("other").unwrap().checks_paused);
            f.runtime.goal_tool_activity(other, "read");
            let independent = f.runtime.reserve_run_id();
            f.runtime
                .bind_thread_root("independent", independent)
                .unwrap();
            assert_eq!(
                f.runtime.goal_thread(independent).as_deref(),
                Some("independent")
            );
            assert!(plan_events(&mut events).is_empty()); // No success notification before commit.
            let replacement = f.runtime.reserve_run_id();
            let rt = &f.runtime;
            let bind = scope.spawn(move || rt.bind_thread_root("thread", replacement).unwrap());
            held.release(PlanWriteResolution::Continue);
            update.join().unwrap();
            bind.join().unwrap();
            assert_eq!(
                f.runtime.goal_thread(replacement).as_deref(),
                Some("thread")
            );
            assert!(
                tools::update(
                    &f.runtime,
                    &f.caller(run, &f.mailbox(run)),
                    addition(1, "pending")
                )
                .is_err()
            );
        });
        let saved = f
            .runtime
            .conversation_plan_history("thread", None, None)
            .unwrap();
        let notifications = plan_events(&mut events);
        assert_eq!(notifications.len(), 1);
        assert_eq!(
            notifications[0].kind,
            event_bus::EventKind::ConversationPlan(
                saved
                    .entries
                    .last()
                    .unwrap()
                    .event
                    .clone()
                    .expect("saved event missing")
            )
        );
        f.stop(run).await;
        f.stop(other).await;
    }
}

#[tokio::test]
async fn commit_stop_never_reports_success_and_reread_resolves_unknown_outcome() {
    for (point, revision) in [
        (PlanWritePoint::BeforeCommit, 0),
        (PlanWritePoint::AfterCommit, 1),
    ] {
        let f = Fixture::new();
        let run = f.start("thread").await;
        let gate = PlanWriteGate::new();
        f.storage
            .handle()
            .install_plan_write_gate(gate.clone())
            .unwrap();
        gate.arm(point);
        let mut events = f.runtime.event_bus().subscribe();
        std::thread::scope(|scope| {
            let call = scope.spawn(|| {
                tools::update(
                    &f.runtime,
                    &f.caller(run, &f.mailbox(run)),
                    addition(0, "pending"),
                )
            });
            let mut held = HeldWriter(Some(gate.clone()));
            assert_eq!(gate.wait_reached(), point);
            held.release(PlanWriteResolution::Stop);
            assert!(
                call.join()
                    .unwrap()
                    .unwrap_err()
                    .starts_with("storage_unavailable:")
            );
        });
        assert!(plan_events(&mut events).is_empty());
        let authoritative = read(&f, run);
        assert_eq!(authoritative["revision"], revision);
        assert_eq!(
            authoritative["nodes"].as_array().unwrap().len(),
            revision as usize
        );
        assert_eq!(
            f.runtime
                .conversation_plan_history("thread", None, None)
                .unwrap()
                .entries
                .len(),
            revision as usize
        );
        if revision == 1 {
            assert!(
                tools::update(
                    &f.runtime,
                    &f.caller(run, &f.mailbox(run)),
                    addition(0, "pending")
                )
                .unwrap_err()
                .starts_with("stale_revision:")
            );
            assert_eq!(read(&f, run), authoritative);
        }
        f.stop(run).await;
    }
}
