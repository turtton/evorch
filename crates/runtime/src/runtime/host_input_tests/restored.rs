use super::*;
use crate::restore::RestoredState;

#[tokio::test]
async fn restored_delivery_panic_seals_followup_before_error() {
    let fixture = fixture(true);
    fixture.wait_running().await;
    fixture.entered.notified().await;
    fixture
        .runtime
        .stop(fixture.run, crate::StopScope::Subtree)
        .unwrap();
    fixture.runtime.wait(fixture.run).await.unwrap();
    let runtime = &fixture.runtime;
    let trigger = AgentMessage {
        message_id: "restore-panic".into(),
        sender_run_id: fixture.run.to_string(),
        recipient_run_id: fixture.run.to_string(),
        kind: AgentMessageKind::Send,
        content: "internal trigger".into(),
        reply_to: None,
    };
    let store = runtime.shared.run_store.get().unwrap();
    let record = store
        .run_contexts_for(&[fixture.run.to_string()])
        .unwrap()
        .pop()
        .unwrap();
    let restored = RestoredState::for_conversation(&record).unwrap();
    {
        let mut intents = runtime.shared.spawn_intents.lock().unwrap();
        // An explicit restored registration starts a new execution.
        intents.remove(&fixture.run);
        let mut admissions = runtime.shared.admissions.lock().unwrap();
        let execution =
            AgentRuntime::prepare_restored_execution(fixture.run, None, &intents).unwrap();
        AgentRuntime::install_restored_execution(
            fixture.run,
            None,
            &mut intents,
            &mut admissions,
            &execution,
        );
        runtime
            .register_restored(
                &mut lock_runs(&runtime.shared.runs),
                &intents,
                execution,
                (
                    fixture.run,
                    None,
                    Role::Worker,
                    RunConfig {
                        interactive: true,
                        keep_alive: true,
                        ..Default::default()
                    },
                    fixture.run,
                ),
                restored,
                &trigger,
                None,
            )
            .unwrap();
    }
    runtime
        .send_internal_message(fixture.run, "resume provider".into())
        .unwrap();
    fixture.entered.notified().await;
    runtime
        .send_message_with_images(
            fixture.run,
            "followup".into(),
            Vec::new(),
            Some("restored-followup".into()),
        )
        .unwrap();
    assert_eq!(
        fixture.receipt("restored-followup").unwrap().status,
        HostInputStatus::Accepted
    );
    fixture.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.phase() == AgentRunPhase::Error {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(runtime.entry(fixture.run).unwrap().user_inbox.sealed());
    assert_eq!(
        fixture.receipt("restored-followup").unwrap().status,
        HostInputStatus::Unknown
    );
}

#[tokio::test]
async fn current_admission_generation_outlives_old_terminal_entry() {
    let fixture = fixture(false);
    fixture.wait_running().await;
    fixture
        .runtime
        .stop(fixture.run, crate::StopScope::Subtree)
        .unwrap();
    fixture.runtime.wait(fixture.run).await.unwrap();
    assert!(!fixture.runtime.host_input_run_is_live(fixture.run));
    {
        let mut intents = fixture.runtime.shared.spawn_intents.lock().unwrap();
        intents.remove(&fixture.run);
    }
    fixture.runtime.reserve_admission(fixture.run, None, None);
    assert!(fixture.runtime.host_input_run_is_live(fixture.run));
}
