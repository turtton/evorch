use super::*;
fn seed_lane(f: &Fixture, lead: RunId, index: usize, run: RunId) -> LaneRecord {
    f.storage
        .handle()
        .create_lane(NewLane {
            lane_id: format!("recovery-{index:03}"),
            thread_id: format!("recovery-thread-{index:03}"),
            origin_thread_id: "lead".into(),
            origin_run_id: lead.to_string(),
            creation_key: format!("recovery-call-{index}"),
            brief: brief(),
            start_seed: serde_json::to_string(&StartSeed {
                run_id: run,
                project_root: None,
                requires_ownership: false,
            })
            .unwrap(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap()
        .lane
}
fn sql(f: &Fixture) -> rusqlite::Connection {
    rusqlite::Connection::open(f._dir.path().join("lanes.db")).unwrap()
}
fn corrupt_graph(f: &Fixture, runs: &[RunId]) {
    // Broken payloads must not erase readable relational identities.
    let mut db = sql(f);
    let tx = db.transaction().unwrap();
    tx.execute(
        "INSERT INTO recovery_manifests VALUES('blocked',1,1,0,?1,0,'[]')",
        [runs.len()],
    )
    .unwrap();
    for run in runs {
        tx.execute(
            "INSERT INTO recovery_members VALUES('blocked',?1,'old',0,NULL,1,'broken')",
            [run.to_string()],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}
#[tokio::test]
async fn graph_exclusion_precedes_limit_and_only_the_201st_lane_is_admitted() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let excluded = (0..200)
        .map(|i| {
            let run = f.runtime.shared.run_ids.next();
            seed_lane(&f, lead, i, run);
            run
        })
        .collect::<Vec<_>>();
    let run = f.runtime.shared.run_ids.next();
    seed_lane(&f, lead, 200, run);
    corrupt_graph(&f, &excluded);
    f.runtime.fence_graph_lane_recovery().unwrap();
    let before = f.model.admissions.lock().unwrap().len();
    assert_eq!(f.storage.handle().unstarted_lanes(1000).unwrap().len(), 200);
    assert_eq!(
        f.storage
            .handle()
            .unstarted_lanes_excluding(1, f.runtime.lane_recovery_exclusions())
            .unwrap()[0]
            .lane_id,
        "recovery-200"
    );
    assert_eq!(f.runtime.recover_unstarted_lanes(None).unwrap(), [run]);
    f.phase(run, AgentRunPhase::Waiting).await;
    assert_eq!(
        &f.model.admissions.lock().unwrap()[before..],
        &[run.to_string()]
    );
    assert!(
        f.storage
            .handle()
            .unstarted_lanes_excluding(1, f.runtime.lane_recovery_exclusions())
            .unwrap()
            .is_empty()
    );
    assert!(f.runtime.recover_unstarted_lanes(None).unwrap().is_empty());
    let owner = Arc::new(
        crate::ownership::OwnerHost::open(
            &f._dir.path().join("ownership"),
            Default::default(),
            Arc::new(EventBus::new(32)),
        )
        .unwrap(),
    );
    let admissions = f.model.admissions.lock().unwrap().len();
    let requests = f.model.requests.lock().unwrap().len();
    // The owned retry future must terminate despite 200 excluded pending rows.
    // Await its semantic completion; runner-level hang detection catches regressions.
    f.runtime.prepare_lane_recovery(&owner).unwrap().await;
    assert_eq!(f.model.admissions.lock().unwrap().len(), admissions);
    assert_eq!(f.model.requests.lock().unwrap().len(), requests);
    assert_eq!(
        f.model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _, _)| excluded.iter().any(|run| run.to_string() == *id))
            .count(),
        0
    );
    assert_eq!(f.storage.handle().unstarted_lanes(1000).unwrap().len(), 200);
    f.cancel(run).await;
    f.cancel(lead).await;
}
#[tokio::test]
async fn stopped_graph_lane_is_rejected_before_ownership_or_ledger_changes() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let run = f.runtime.shared.run_ids.next();
    let lane = seed_lane(&f, lead, 0, run);
    corrupt_graph(&f, &[run]);
    f.storage
        .handle()
        .block_lane_start(&lane.lane_id, storage::lanes::LANE_START_OPERATOR_STOP, 2)
        .unwrap();
    f.runtime.fence_graph_lane_recovery().unwrap();
    let before = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    let owner = Arc::new(
        crate::ownership::OwnerHost::open(
            &f._dir.path().join("ownership"),
            Default::default(),
            Arc::new(EventBus::new(32)),
        )
        .unwrap(),
    );
    assert_eq!(
        f.runtime
            .resume_stopped_lane(&lane.lane_id, Some(&owner))
            .unwrap_err()
            .code,
        "graph_recovery_required"
    );
    assert!(matches!(
        owner.attach(&lane.thread_id),
        Err(crate::ownership::RegistryError::Absent)
    ));
    assert_eq!(
        f.storage.handle().lane(&lane.lane_id).unwrap().unwrap(),
        before
    );
    let seed = serde_json::from_str(&lane.start_seed).unwrap();
    assert_eq!(
        f.runtime
            .start_recovered(&lane, &seed, None)
            .unwrap_err()
            .code,
        "graph_recovery_required"
    );
    assert!(
        f.runtime
            .recover_unstarted_lanes(Some(&owner))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.storage.handle().lane(&lane.lane_id).unwrap().unwrap(),
        before
    );
    f.cancel(lead).await;
}
#[tokio::test]
async fn deleting_a_manifest_keeps_its_orphan_lane_recoverable() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let run = f.runtime.shared.run_ids.next();
    seed_lane(&f, lead, 0, run);
    corrupt_graph(&f, &[run]);
    let db = sql(&f);
    // A retained intent without a manifest is NOT graph ownership.
    let mut intent = f
        .runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|intent| intent.run_id == lead)
        .unwrap();
    intent.run_id = run;
    intent.incarnation = "old".into();
    intent.wait = crate::quiesce::ResumeWait::AdmissionQueued;
    intent.inputs.clear();
    intent.checkpoint = None;
    intent.snapshot = None;
    intent.provider_input = None;
    intent.initial_input = Some(crate::quiesce::DurableInput {
        id: "orphan".into(),
        text: "recover".into(),
        images: vec![],
        trusted_user: true,
    });
    db.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'old',?2)",
        rusqlite::params![run.to_string(), serde_json::to_string(&intent).unwrap()],
    )
    .unwrap();
    db.execute("DELETE FROM recovery_manifests", []).unwrap();
    assert!(
        f.runtime
            .recovery_plan()
            .unwrap()
            .report
            .skipped
            .iter()
            .any(|entry| entry.runs == [run]
                && entry.reason == crate::quiesce::RecoveryReason::NoCommittedManifest)
    );
    f.runtime.fence_graph_lane_recovery().unwrap();
    assert!(f.runtime.lane_recovery_exclusions().is_empty());
    assert_eq!(f.runtime.recover_unstarted_lanes(None).unwrap(), [run]);
    f.phase(run, AgentRunPhase::Waiting).await;
    f.cancel(run).await;
    f.cancel(lead).await;
}

#[tokio::test]
async fn idempotent_open_of_a_graph_lane_never_creates_an_owner_or_admits() {
    let mut f = Fixture::new();
    let owner = Arc::new(
        crate::ownership::OwnerHost::open(
            &f._dir.path().join("ownership"),
            Default::default(),
            Arc::new(EventBus::new(32)),
        )
        .unwrap(),
    );
    let permit = owner.start("lead").unwrap();
    let lead = f
        .runtime
        .delegate_chat(
            "lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                ownership: Some(permit),
                interactive: true,
                keep_alive: true,
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    *f.model.lead.lock().unwrap() = Some(lead.to_string());
    let run = f.runtime.shared.run_ids.next();
    let lane = seed_lane(&f, lead, 0, run);
    corrupt_graph(&f, &[run]);
    f.runtime.fence_graph_lane_recovery().unwrap();
    let before = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    let admissions = f.model.admissions.lock().unwrap().len();
    let requests = f.model.requests.lock().unwrap().len();
    for _ in 0..2 {
        assert_eq!(
            f.runtime
                .open_lane(lead, "recovery-call-0", brief())
                .unwrap_err()
                .code,
            "graph_recovery_required"
        );
    }
    assert!(matches!(
        owner.attach(&lane.thread_id),
        Err(crate::ownership::RegistryError::Absent)
    ));
    assert_eq!(
        f.storage.handle().lane(&lane.lane_id).unwrap().unwrap(),
        before
    );
    assert_eq!(f.storage.handle().unstarted_lanes(200).unwrap(), [before]);
    assert!(f.runtime.shared.lanes.opening.lock().unwrap().is_empty());
    assert!(
        f.runtime
            .shared
            .lanes
            .creation_intents
            .lock()
            .unwrap()
            .is_empty()
    );
    assert!(f.runtime.admission_snapshot(run).is_none());
    assert!(f.runtime.inspect_agent(run).is_err());
    assert_eq!(f.model.admissions.lock().unwrap().len(), admissions);
    assert_eq!(f.model.requests.lock().unwrap().len(), requests);
    f.cancel(lead).await;
}
