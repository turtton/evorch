//! A question the offline cleanup cancelled must not wake or feed the lead it was routed to:
//! the cleanup makes its pending notifications obsolete in the same transaction, so after a
//! restart the delivery tick finds nothing to claim for it.
use super::*;
use event_bus::lane::QuestionResponder;

struct Lead {
    target: LaneDeliveryTarget,
    restores: AtomicUsize,
    unreachable: AtomicBool,
}
impl LaneDeliveryHost for Lead {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread == "delivery-lead" && !self.unreachable.load(Ordering::SeqCst) {
            Ok(self.target.clone())
        } else {
            Err(LaneDeliveryNack::UnknownThread)
        }
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| LaneDeliveryNack::RestoreFailed)
    }
    fn now_ns(&self) -> i64 {
        100
    }
}

/// Runs the whole story and returns (lead restores, provider requests, pending question
/// notifications for the lead) observed by the delivery tick after the restart.
async fn restart_and_tick(apply_cleanup: bool) -> (usize, usize, i64) {
    let mut f = Fixture::new();
    let (lead, old) = f.lead().await;
    let lane = f.lane(lead, "cancelled-question").await;
    f.finish(lead).await;
    // The lead is finished and unreachable while the lane asks it a question, so the
    // question's notification stays pending (nothing could deliver it).
    f.model.next_tools.lock().unwrap().insert(
        lane.root_run_id.to_string(),
        ContentBlock::ToolUse {
            id: "ask-bound".into(),
            name: "ask_user".into(),
            input: serde_json::json!({"title":"Which scope?","blocking":false}),
        },
    );
    let host = Arc::new(Lead {
        target: old.target.clone(),
        restores: AtomicUsize::new(0),
        unreachable: AtomicBool::new(true),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    f.runtime
        .send_message(lane.root_run_id, "ask the managing lead".into())
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let question = f
        .storage
        .handle()
        .lane_questions("delivery-lead".into(), None)
        .unwrap()
        .questions
        .remove(0);
    assert!(matches!(
        question.responder,
        Some(QuestionResponder::Lead { .. })
    ));

    let waiting: i64 = rusqlite::Connection::open(&f.config.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE event_kind='question' AND state='pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        waiting, 1,
        "the lead could not be reached, so the question is still pending"
    );
    let Fixture {
        runtime,
        storage,
        config,
        _dir,
        ..
    } = f;
    let target = old.target.clone();
    drop(runtime);
    storage.close();

    let config_dir = _dir.path().to_path_buf();
    std::fs::write(config_dir.join("sidebar.json"), r#"{"threads":[]}"#).unwrap();
    if apply_cleanup {
        let preview = ::storage::maintenance::preview_archive_blocker_cleanup(
            &::storage::maintenance::CleanupPreviewOptions {
                db: config.db_path.clone(),
                config_dir: config_dir.clone(),
                sidebar: config_dir.join("sidebar.json"),
            },
        )
        .unwrap();
        assert!(
            preview.apply_allowed,
            "{:?}",
            preview.manifest.unprocessable
        );
        assert_eq!(preview.counts.questions_to_cancel, 1);
        assert_eq!(preview.counts.question_notifications_to_obsolete, 1);
        let plan = config_dir.join("plan.json");
        std::fs::write(&plan, serde_json::to_vec(&preview).unwrap()).unwrap();
        ::storage::maintenance::apply_archive_blocker_cleanup(
            &::storage::maintenance::CleanupApplyOptions {
                db: config.db_path.clone(),
                config_dir: config_dir.clone(),
                ownership_root: config_dir.join("threads"),
                sidebar: config_dir.join("sidebar.json"),
                plan,
                backup: config_dir.join("backup.db"),
            },
        )
        .unwrap();
    }

    // A restart: a new runtime over the same database, with the lead reachable again.
    let storage = ::storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(Model::default());
    let bus = Arc::new(EventBus::new(16384));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let host = Arc::new(Lead {
        target,
        restores: AtomicUsize::new(0),
        unreachable: AtomicBool::new(false),
    });
    *runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    runtime.drive_pending_lane_deliveries().await.unwrap();
    let pending: i64 = rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM lane_notifications WHERE event_kind='question' AND state='pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let requests = model.requests.lock().unwrap().len();
    (host.restores.load(Ordering::SeqCst), requests, pending)
}

#[tokio::test]
async fn a_cancelled_question_does_not_wake_the_lead_after_a_restart() {
    // Control: without the cleanup the pending question wakes the lead.
    let (restores, requests, pending) = restart_and_tick(false).await;
    assert!(
        restores >= 1 && requests >= 1,
        "control: {restores} restores {requests} requests"
    );
    assert_eq!(pending, 0, "the control's question was delivered");

    // After the cleanup nothing is claimed for it: no restore, no provider request.
    let (restores, requests, pending) = restart_and_tick(true).await;
    assert_eq!((restores, requests, pending), (0, 0, 0));
}
