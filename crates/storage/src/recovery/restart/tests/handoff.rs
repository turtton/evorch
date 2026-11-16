use super::*;
use crate::learning_jobs::*;
use crate::recovery::restart::{handoff::*, tools::*};

fn origin(instance: &str) -> LearningOrigin {
    LearningOrigin {
        kind: LearningOriginKind::Core,
        instance: instance.into(),
        pid: 42,
        start_ticks: Some(7),
        boot_id: Some("boot".into()),
        pid_ns: Some("pidns".into()),
        config_identity: Some("config".into()),
    }
}
fn admit(db: &Database, id: &str, who: &str) {
    crate::learning_jobs::write(
        &db.conn,
        crate::learning_jobs::Write::Admit(
            Box::new(NewLearningJob {
                task_id: id.into(),
                roots: vec![id.into()],
                goal: false,
                daily_limit: 20,
                project: "p".into(),
                source_run_id: id.into(),
                origin: origin(who),
                prompt: "fixed prompt".into(),
                evidence_json: "{}".into(),
            }),
            0,
        ),
    )
    .unwrap();
}
fn change(db: &Database, id: &str, change: LearningAttemptChange) {
    let key = db.learning_job(id).unwrap().unwrap().key();
    assert_eq!(
        crate::learning_jobs::write(
            &db.conn,
            crate::learning_jobs::Write::Attempt(Box::new(LearningAttemptWrite { key, change }), 0)
        )
        .unwrap(),
        crate::learning_jobs::Written::Applied(true)
    );
}
fn interrupted(db: &Database, id: &str) {
    change(db, id, LearningAttemptChange::Start);
    change(
        db,
        id,
        LearningAttemptChange::Interrupt {
            suspension_id: "suspend".into(),
        },
    );
}
fn capture(db: &Database) -> RestartTransaction {
    step(db, create(db), RestartMutation::BeginCapture(vec![]))
}
fn armed(db: &Database, t: RestartTransaction) -> RestartTransaction {
    let t = step(db, t, RestartMutation::Commit);
    step(
        db,
        t,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    )
}
fn resume(id: &str) -> RestartMutation {
    RestartMutation::ResumeLearning {
        task_id: id.into(),
        to: origin("new"),
    }
}

#[test]
fn planned_learning_membership_is_complete_exact_and_requeues_once_after_ready() {
    let db = Database::open_in_memory().unwrap();
    admit(&db, "active", "old");
    interrupted(&db, "active");
    admit(&db, "late-pending", "old");
    admit(&db, "foreign", "gui");
    let t = capture(&db);
    let inventory = db
        .restart_learning_inventory(origin("old"), "suspend".into())
        .unwrap();
    assert_eq!(inventory.members.len(), 2);
    assert_eq!(inventory.members[1].key.attempt, 0);
    let mut incomplete = inventory.clone();
    incomplete.members.pop();
    assert!(
        submit(
            &db,
            Some(t.revision),
            RestartMutation::LearningInventory(incomplete)
        )
        .is_err()
    );
    let t = lost_ack(&db, Some(t.revision), || {
        RestartMutation::LearningInventory(inventory.clone())
    });
    let t = armed(&db, t);
    let t = step(&db, t, adopt());
    assert!(submit(&db, Some(t.revision), resume("active")).is_err());
    let t = step(&db, t, RestartMutation::Ready { at_ms: 1 });
    let t = lost_ack(&db, Some(t.revision), || resume("active"));
    let job = db.learning_job("active").unwrap().unwrap();
    assert_eq!(job.state, LearningJobState::Pending);
    assert_eq!(job.attempt, 1);
    assert_eq!(job.origin.instance, "new");
    assert_eq!(
        db.learning_job_evidence("active").unwrap().unwrap().prompt,
        "fixed prompt"
    );
    change(&db, "active", LearningAttemptChange::Start);
    let started = db.learning_job("active").unwrap().unwrap();
    assert_eq!(started.attempt, 2);
    let t = step(&db, t, resume("active"));
    assert_eq!(db.learning_job("active").unwrap().unwrap(), started);
    step(&db, t, resume("late-pending"));
    assert_eq!(db.learning_job("late-pending").unwrap().unwrap().attempt, 0);
    assert_eq!(
        db.learning_job("foreign").unwrap().unwrap().origin.instance,
        "gui"
    );
}

#[test]
fn learning_capture_refuses_started_or_changed_members_without_taking_foreign_origin() {
    let db = Database::open_in_memory().unwrap();
    admit(&db, "job", "old");
    change(&db, "job", LearningAttemptChange::Start);
    assert!(
        db.restart_learning_inventory(origin("old"), "suspend".into())
            .is_err()
    );
    change(
        &db,
        "job",
        LearningAttemptChange::Interrupt {
            suspension_id: "suspend".into(),
        },
    );
    let t = capture(&db);
    let inventory = db
        .restart_learning_inventory(origin("old"), "suspend".into())
        .unwrap();
    let t = step(&db, t, RestartMutation::LearningInventory(inventory));
    let t = armed(&db, t);
    let t = step(&db, t, adopt());
    let t = step(&db, t, RestartMutation::Ready { at_ms: 1 });
    change(
        &db,
        "job",
        LearningAttemptChange::Requeue {
            suspension_id: "suspend".into(),
            to: origin("other"),
        },
    );
    let other = db.learning_job("job").unwrap().unwrap();
    assert!(submit(&db, Some(t.revision), resume("job")).is_err());
    assert_eq!(db.learning_job("job").unwrap().unwrap(), other);
}

#[test]
fn cold_learning_is_terminal_and_attempt_limit_cannot_be_reset_by_handoff() {
    for cold in [false, true] {
        let db = Database::open_in_memory().unwrap();
        admit(&db, "job", "old");
        for i in 0..3 {
            interrupted(&db, "job");
            if i < 2 {
                change(
                    &db,
                    "job",
                    LearningAttemptChange::Requeue {
                        suspension_id: "suspend".into(),
                        to: origin("old"),
                    },
                );
            }
        }
        let t = capture(&db);
        let inventory = db
            .restart_learning_inventory(origin("old"), "suspend".into())
            .unwrap();
        let t = step(&db, t, RestartMutation::LearningInventory(inventory));
        let t = armed(&db, t);
        if cold {
            crate::recovery::write(
                &db.conn,
                crate::recovery::Write::RestartColdQuarantine {
                    id: "restart".into(),
                    expected_revision: t.revision,
                },
            )
            .unwrap();
            assert_eq!(
                db.learning_job("job").unwrap().unwrap().state,
                LearningJobState::Interrupted
            );
            assert!(db.learning_job_evidence("job").unwrap().is_some());
            // Liveness/config checks belong to the caller of the separate learning write.
            let t = db.restart_transaction("restart").unwrap().unwrap();
            step(&db, t, RestartMutation::QuarantineLearningCold);
        } else {
            let t = step(&db, t, adopt());
            let t = step(&db, t, RestartMutation::Ready { at_ms: 1 });
            step(&db, t, resume("job"));
        }
        let job = db.learning_job("job").unwrap().unwrap();
        assert!(job.state.is_terminal());
        assert_eq!(job.attempt, 3);
        assert!(db.learning_job_evidence("job").unwrap().is_none());
    }
}

fn tool(name: &str, version: u32) -> serde_json::Value {
    json!({"name":name,"description":format!("v{version}"),"input_schema":{"type":"object"}})
}
fn snapshots() -> (ToolSnapshot, ToolSnapshot) {
    let old = vec![tool("read", 1), tool("shell", 1)];
    let new = vec![tool("read", 2), tool("shell", 1)];
    (
        ToolSnapshot {
            effective: old.clone(),
            builtins: old,
            stable_context: "stable".into(),
        },
        ToolSnapshot {
            effective: new.clone(),
            builtins: new,
            stable_context: "stable".into(),
        },
    )
}
#[test]
fn tool_attribution_rejects_same_build_external_permissions_and_unexplained_order_changes() {
    let (old, new) = snapshots();
    let compare = |a, b, build: &str| {
        RestartToolsEvidence::compare("run".into(), "old".into(), "a".into(), build.into(), a, b)
    };
    assert!(compare(old.clone(), new.clone(), "a").is_err());
    assert!(compare(old.clone(), new.clone(), "b").unwrap().boundary);
    assert!(!compare(old.clone(), old.clone(), "b").unwrap().boundary);
    let mut changed = new.clone();
    changed.stable_context = "permission change".into();
    assert!(compare(old.clone(), changed, "b").is_err());
    let mut external = new.clone();
    external.effective.push(tool("mcp", 1));
    assert!(compare(old.clone(), external, "b").is_err());
    let mut reorder = new.clone();
    reorder.effective.reverse();
    assert!(compare(old.clone(), reorder, "b").is_err());
    let mut reorder = new.clone();
    reorder.effective.reverse();
    reorder.builtins.reverse();
    assert!(compare(old.clone(), reorder, "b").unwrap().boundary);
    let mut availability = new;
    availability.effective.pop();
    assert!(compare(old, availability, "b").is_err());
    let (mut old, mut new) = snapshots();
    old.effective.push(tool("mcp", 1));
    new.effective.insert(0, tool("mcp", 1));
    assert!(
        compare(old, new, "b").is_err(),
        "external placement is not a compiled change"
    );
}

#[test]
fn cold_graph_quarantine_does_not_decode_corrupt_learning_members_or_mutate_unknown_jobs() {
    let db = Database::open_in_memory().unwrap();
    admit(&db, "job", "old");
    interrupted(&db, "job");
    let t = capture(&db);
    let inventory = db
        .restart_learning_inventory(origin("old"), "suspend".into())
        .unwrap();
    let t = step(&db, t, RestartMutation::LearningInventory(inventory));
    let t = armed(&db, t);
    db.conn
        .execute(
            "UPDATE restart_learning_members SET member_json='broken'",
            [],
        )
        .unwrap();
    crate::recovery::write(
        &db.conn,
        crate::recovery::Write::RestartColdQuarantine {
            id: "restart".into(),
            expected_revision: t.revision,
        },
    )
    .unwrap();
    assert_eq!(
        db.restart_transaction("restart").unwrap().unwrap().phase,
        RestartPhase::Invalidated
    );
    assert_eq!(
        db.learning_job("job").unwrap().unwrap().state,
        LearningJobState::Interrupted
    );
    assert!(db.learning_job_evidence("job").unwrap().is_some());
    let disposition = || {
        db.conn
            .query_row(
                "SELECT disposition FROM restart_learning_members",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(disposition(), "pending");
    let t = db.restart_transaction("restart").unwrap().unwrap();
    step(&db, t, RestartMutation::QuarantineLearningCold);
    assert_eq!(disposition(), "quarantined");
    // Invalid membership cannot authorize an Abandon; the origin-aware Interrupted pass
    // decides separately whether the unidentifiable job can be cleaned up.
    assert_eq!(
        db.learning_job("job").unwrap().unwrap().state,
        LearningJobState::Interrupted
    );
    assert!(db.learning_job_evidence("job").unwrap().is_some());
}

#[test]
fn tool_boundary_consumption_is_per_incarnation_and_lost_ack_never_reissues_it() {
    let db = Database::open_in_memory().unwrap();
    seed(&db, "run");
    let t = create(&db);
    let t = step(&db, t, RestartMutation::BeginCapture(vec![expected("run")]));
    let (old, new) = snapshots();
    let t = step(
        &db,
        t,
        RestartMutation::CaptureTools {
            run_id: "run".into(),
            incarnation: "old".into(),
            snapshot: old.clone(),
        },
    );
    let t = step(&db, t, RestartMutation::SaveComponent(component("run")));
    let t = armed(&db, t);
    let t = step(&db, t, adopt());
    let e =
        RestartToolsEvidence::compare("run".into(), "old".into(), "a".into(), "b".into(), old, new)
            .unwrap();
    let t = lost_ack(&db, Some(t.revision), || {
        RestartMutation::Tools(Box::new(e.clone()))
    });
    let consume = || RestartMutation::ConsumeTools {
        run_id: "run".into(),
        incarnation: "old".into(),
    };
    assert!(submit(&db, Some(t.revision), consume()).is_err());
    let t = step(&db, t, RestartMutation::Ready { at_ms: 1 });
    let t = lost_ack(&db, Some(t.revision), consume);
    assert!(
        db.restart_tools_evidence("restart", "run", "old")
            .unwrap()
            .unwrap()
            .1
    );
    assert!(submit(&db, Some(t.revision), consume()).is_err());
    assert!(
        db.restart_tools_evidence("restart", "run", "later")
            .unwrap()
            .is_none()
    );
}

#[test]
fn boot_diagnostic_time_is_fixed_and_codes_are_bounded() {
    let db = Database::open_in_memory().unwrap();
    let t = armed(&db, capture(&db));
    let progress = RestartBootProgress {
        restarting_at_ms: 10,
        failure: None,
        exit_code: None,
        diagnostic_revision: 0,
    };
    let t = step(&db, t, RestartMutation::BootProgress(progress.clone()));
    let mut wrong = progress.clone();
    wrong.restarting_at_ms = 20;
    wrong.diagnostic_revision = 1;
    assert!(submit(&db, Some(t.revision), RestartMutation::BootProgress(wrong)).is_err());
    let failure = RestartBootProgress {
        failure: Some(BootFailure::AckUnknown),
        exit_code: Some(72),
        diagnostic_revision: 1,
        ..progress
    };
    lost_ack(&db, Some(t.revision), || {
        RestartMutation::BootProgress(failure.clone())
    });
    assert_eq!(db.restart_boot_progress("restart").unwrap(), Some(failure));
}
