//! Synchronous failpoints distinguish rollback from lost acknowledgements.
use super::*;
use serde_json::json;
mod handoff;
fn metadata(id: &str) -> RestartMetadata {
    RestartMetadata {
        restart_id: id.into(),
        capture_id: format!("capture/{id}"),
        config_identity: "config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "a".into(),
        successor_build: "b".into(),
        target_identity: "target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 62,
        old_builtin_tool_names: BTreeSet::from(["read".into(), "shell".into()]),
        old_compiled_catalog_fingerprint: Some("catalog-a".into()),
        new_compiled_catalog_fingerprint: Some("catalog-b".into()),
        old_effective_tools_fingerprint: Some("tools".into()),
        new_effective_tools_fingerprint: Some("tools".into()),
    }
}
fn submit(
    db: &Database,
    rev: Option<u64>,
    m: RestartMutation,
) -> Result<RestartTransaction, StorageError> {
    match crate::recovery::write(
        &db.conn,
        Write::Restart {
            id: "restart".into(),
            expected_revision: rev,
            mutation: m,
        },
    )? {
        Written::Restart(t) => Ok(*t),
        _ => unreachable!(),
    }
}
fn step(db: &Database, t: RestartTransaction, m: RestartMutation) -> RestartTransaction {
    submit(db, Some(t.revision), m).unwrap()
}
fn create(db: &Database) -> RestartTransaction {
    submit(
        db,
        None,
        RestartMutation::Create(Box::new(metadata("restart"))),
    )
    .unwrap()
}
fn expected(id: &str) -> RestartExpectedMember {
    RestartExpectedMember {
        run_id: id.into(),
        incarnation: "old".into(),
        root_run_id: id.into(),
        thread_id: Some(format!("thread/{id}")),
        intent_revision: 0,
        snapshot: None,
        owner_id: Some("owner".into()),
        owner_generation: Some(7),
        lane_id: Some(id.into()),
    }
}
fn seed(db: &Database, id: &str) {
    db.conn.execute("INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'old',?2)",params![id,json!({"run_id":id,"incarnation":"old","active":true,"stop_reason":null,"tool_intent":[],"initial_input":{"id":"initial"},"wait":{"kind":"admission_queued"}}).to_string()]).unwrap();
}
fn component(id: &str) -> RecoveryManifestRecord {
    RecoveryManifestRecord {
        manifest_id: format!("component/{id}"),
        schema_version: 1,
        committed: false,
        superseded: false,
        edges: vec![],
        members: vec![RecoveryMemberRecord {
            run_id: id.into(),
            incarnation: "old".into(),
            intent_revision: 0,
            snapshot: None,
            resume: true,
            record_json: "{}".into(),
        }],
    }
}
fn lost_ack(
    db: &Database,
    rev: Option<u64>,
    make: impl Fn() -> RestartMutation,
) -> RestartTransaction {
    let before = db.restart_transaction("restart").unwrap();
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(false)));
    assert!(submit(db, rev, make()).is_err());
    assert_eq!(db.restart_transaction("restart").unwrap(), before);
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(true)));
    assert!(submit(db, rev, make()).is_err());
    let after = db.restart_transaction("restart").unwrap().unwrap();
    assert_eq!(after.revision, rev.map_or(0, |r| r + 1));
    assert!(
        submit(db, rev, make()).is_err(),
        "unknown ACK must read back, not reapply"
    );
    assert_eq!(
        db.restart_transaction("restart").unwrap(),
        Some(after.clone())
    );
    after
}
fn adopt() -> RestartMutation {
    RestartMutation::Adopt {
        nonce_hash: "nonce".into(),
        successor_epoch: 2,
        config_identity: "config".into(),
        successor_build: "b".into(),
    }
}
#[test]
fn restart_normal_cas_before_commit_and_lost_ack_including_zero_members() {
    let db = Database::open_in_memory().unwrap();
    let t = lost_ack(&db, None, || {
        RestartMutation::Create(Box::new(metadata("restart")))
    });
    let t = lost_ack(&db, Some(t.revision), || {
        RestartMutation::BeginCapture(vec![])
    });
    let t = lost_ack(&db, Some(t.revision), || RestartMutation::Commit);
    let t = lost_ack(&db, Some(t.revision), || RestartMutation::Arm {
        nonce_hash: "nonce".into(),
    });
    let t = lost_ack(&db, Some(t.revision), adopt);
    let t = lost_ack(&db, Some(t.revision), || RestartMutation::Ready {
        at_ms: 123,
    });
    assert_eq!(t.phase, RestartPhase::Completed);
    assert_eq!(t.ready_at_ms, Some(123));
    assert_eq!(step(&db, t.clone(), RestartMutation::QuarantineCold), t);
}
#[test]
fn restart_component_owner_and_terminal_cas_are_atomic() {
    for end in ["abort", "invalidate", "supersede", "cold"] {
        let db = Database::open_in_memory().unwrap();
        let t = create(&db);
        lost_ack(&db, Some(t.revision), || match end {
            "abort" => RestartMutation::Abort {
                reason: "operator".into(),
            },
            "invalidate" => RestartMutation::Invalidate {
                reason: "owner lost".into(),
            },
            "supersede" => RestartMutation::Supersede {
                reason: "new".into(),
            },
            _ => RestartMutation::QuarantineCold,
        });
    }
    let db = Database::open_in_memory().unwrap();
    seed(&db, "run");
    let t = create(&db);
    let t = step(&db, t, RestartMutation::BeginCapture(vec![expected("run")]));
    let t = lost_ack(&db, Some(t.revision), || {
        RestartMutation::SaveComponent(component("run"))
    });
    let t = step(&db, t, RestartMutation::Commit);
    let t = lost_ack(&db, Some(t.revision), || {
        RestartMutation::OwnerProgress(RestartOwnerProgress {
            thread_id: "thread/run".into(),
            old_owner_id: "owner".into(),
            old_generation: 7,
            active_runs: BTreeSet::from(["run".into()]),
            parked_runs: BTreeSet::new(),
            hold_saved: true,
            hold_read_back: true,
            worker_joined: false,
            socket_unlinked: false,
            hold_removed: false,
        })
    });
    let t = step(
        &db,
        t,
        RestartMutation::Arm {
            nonce_hash: "nonce".into(),
        },
    );
    let t = step(&db, t, adopt());
    assert!(db.has_pending_adopted_restart().unwrap());
    let t = step(&db, t, RestartMutation::Ready { at_ms: 7 });
    assert_eq!(t.phase, RestartPhase::Adopted);
    assert!(
        crate::recovery::write(
            &db.conn,
            Write::Restart {
                id: "next".into(),
                expected_revision: None,
                mutation: RestartMutation::Create(Box::new(metadata("next")))
            }
        )
        .is_err()
    );
    let t = lost_ack(&db, Some(t.revision), || RestartMutation::Dispose {
        manifest_id: "component/run".into(),
        expected_revision: 0,
        disposition: RestartDisposition::Quarantined,
        claim_id: None,
        reason: Some("incompatible".into()),
    });
    assert_eq!(t.phase, RestartPhase::Completed);
    assert_eq!(t.retry_pending(), 0);
    assert!(!db.has_pending_adopted_restart().unwrap());
}
#[test]
fn restart_partial_capture_never_commits_and_exclusions_survive_manifest_loss() {
    let db = Database::open_in_memory().unwrap();
    seed(&db, "a");
    seed(&db, "b");
    let t = create(&db);
    let t = step(
        &db,
        t,
        RestartMutation::BeginCapture(vec![expected("a"), expected("b")]),
    );
    let t = step(&db, t, RestartMutation::SaveComponent(component("a")));
    assert!(submit(&db, Some(t.revision), RestartMutation::Commit).is_err());
    assert_eq!(
        db.restart_excluded_run_ids().unwrap(),
        BTreeSet::from(["a".into(), "b".into()])
    );
    let t = step(&db, t, RestartMutation::QuarantineCold);
    assert_eq!(t.phase, RestartPhase::Invalidated);
    assert_eq!(t.components[0].disposition, RestartDisposition::Quarantined);
    db.conn
        .execute("DELETE FROM recovery_manifests", [])
        .unwrap();
    assert_eq!(
        db.recovery_owned_run_ids(&BTreeSet::new()).unwrap(),
        BTreeSet::from(["a".into(), "b".into()])
    );
    assert!(
        submit(
            &db,
            None,
            RestartMutation::Create(Box::new(metadata("restart")))
        )
        .is_err()
    );
    let mut m = metadata("other");
    m.capture_id = t.metadata.capture_id;
    assert!(
        crate::recovery::write(
            &db.conn,
            Write::Restart {
                id: "other".into(),
                expected_revision: None,
                mutation: RestartMutation::Create(Box::new(m))
            }
        )
        .is_err()
    );
}
#[test]
fn restart_corrupt_changed_terminal_and_superseded_capture_cannot_adopt() {
    for damage in [
        "member",
        "payload",
        "terminal",
        "revision",
        "supersede",
        "delete",
    ] {
        let db = Database::open_in_memory().unwrap();
        seed(&db, "run");
        let t = create(&db);
        let t = step(&db, t, RestartMutation::BeginCapture(vec![expected("run")]));
        let t = step(&db, t, RestartMutation::SaveComponent(component("run")));
        let t = step(&db, t, RestartMutation::Commit);
        let t = step(
            &db,
            t,
            RestartMutation::Arm {
                nonce_hash: "nonce".into(),
            },
        );
        assert!(
            submit(
                &db,
                Some(t.revision),
                RestartMutation::Adopt {
                    nonce_hash: "wrong".into(),
                    successor_epoch: 2,
                    config_identity: "config".into(),
                    successor_build: "b".into()
                }
            )
            .is_err()
        );
        match damage{"member"=>db.conn.execute("DELETE FROM recovery_members",[]).unwrap(),"payload"=>db.conn.execute("UPDATE restart_expected_members SET member_json='{}'",[]).unwrap(),"terminal"=>db.conn.execute("UPDATE resume_intents SET payload=json_set(payload,'$.stop_reason','terminal')",[]).unwrap(),"revision"=>db.conn.execute("UPDATE resume_intents SET revision=revision+1",[]).unwrap(),"supersede"=>db.conn.execute("UPDATE recovery_manifests SET superseded=1",[]).unwrap(),_=>db.conn.execute("DELETE FROM recovery_manifests",[]).unwrap()};
        assert!(submit(&db, Some(t.revision), adopt()).is_err(), "{damage}");
    }
}
#[test]
fn restart_old_builtin_names_have_count_and_byte_bounds() {
    let db = Database::open_in_memory().unwrap();
    let mut m = metadata("restart");
    m.old_builtin_tool_names = (0..=MAX_BUILTIN_TOOL_NAMES)
        .map(|n| format!("tool-{n}"))
        .collect();
    assert!(submit(&db, None, RestartMutation::Create(Box::new(m))).is_err());
    let mut m = metadata("restart");
    m.old_builtin_tool_names = BTreeSet::from(["x".repeat(MAX_BUILTIN_TOOL_NAME_BYTES + 1)]);
    assert!(submit(&db, None, RestartMutation::Create(Box::new(m))).is_err());
    let t = create(&db);
    assert_eq!(
        t.metadata.old_builtin_tool_names,
        BTreeSet::from(["read".into(), "shell".into()])
    );
    let grant: Option<String> = db
        .conn
        .query_row(
            "SELECT tools_boundary_consumption_json FROM restart_transactions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(grant.is_none());

    // Both upper bounds are inclusive, independently of the rejected +1 cases.
    let bounds_db = Database::open_in_memory().unwrap();
    let mut bounds = metadata("restart");
    bounds.old_builtin_tool_names = (0..MAX_BUILTIN_TOOL_NAMES - 1)
        .map(|n| format!("tool-{n}"))
        .chain(std::iter::once("x".repeat(MAX_BUILTIN_TOOL_NAME_BYTES)))
        .collect();
    let stored = submit(
        &bounds_db,
        None,
        RestartMutation::Create(Box::new(bounds.clone())),
    )
    .unwrap();
    assert_eq!(
        stored.metadata.old_builtin_tool_names,
        bounds.old_builtin_tool_names
    );
}

#[test]
fn restart_minimal_invalidation_survives_corrupt_expected_json_and_unknown_ack() {
    let db = Database::open_in_memory().unwrap();
    let t = create(&db);
    let t = step(&db, t, RestartMutation::BeginCapture(vec![]));
    db.conn
        .execute(
            "UPDATE restart_transactions SET expected_digest='corrupt'",
            [],
        )
        .unwrap();
    assert!(db.restart_transaction("restart").is_err());
    let write = || {
        crate::recovery::write(
            &db.conn,
            Write::RestartInvalidation {
                id: "restart".into(),
                expected_revision: t.revision,
                reason: "corrupt evidence".into(),
            },
        )
    };
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(false)));
    assert!(write().is_err());
    assert_eq!(
        db.restart_revision_phase("restart").unwrap(),
        Some((t.revision, RestartPhase::Capturing))
    );
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(true)));
    assert!(write().is_err());
    assert_eq!(
        db.restart_revision_phase("restart").unwrap(),
        Some((t.revision + 1, RestartPhase::Invalidated))
    );
    assert!(write().is_err());
}

#[test]
fn restart_cold_quarantine_handles_corrupt_evidence_and_lost_ack() {
    let db = Database::open_in_memory().unwrap();
    seed(&db, "a");
    let tx = create(&db);
    let tx = step(&db, tx, RestartMutation::BeginCapture(vec![expected("a")]));
    let tx = step(&db, tx, RestartMutation::SaveComponent(component("a")));
    db.conn
        .execute("UPDATE restart_expected_members SET member_json='{'", [])
        .unwrap();
    assert!(db.restart_transaction("restart").is_err());
    assert_eq!(
        db.cold_restart_candidates().unwrap(),
        vec![("restart".into(), tx.revision)]
    );
    let write = || {
        crate::recovery::write(
            &db.conn,
            Write::RestartColdQuarantine {
                id: "restart".into(),
                expected_revision: tx.revision,
            },
        )
    };
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(false)));
    assert!(write().is_err());
    assert_eq!(
        db.restart_revision_phase("restart").unwrap(),
        Some((tx.revision, RestartPhase::Capturing))
    );
    crate::recovery::RESTART_WRITE_FAULT.with(|f| f.set(Some(true)));
    assert!(write().is_err());
    assert_eq!(
        db.restart_revision_phase("restart").unwrap(),
        Some((tx.revision + 1, RestartPhase::Invalidated))
    );
    assert!(db.cold_restart_candidates().unwrap().is_empty());
    let disposition: String = db
        .conn
        .query_row("SELECT disposition FROM restart_components", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(disposition, "quarantined");
    assert!(db.restart_excluded_run_ids().unwrap().contains("a"));
    assert!(write().is_err());
}
