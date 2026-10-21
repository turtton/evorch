//! ADR 0037: metadata, mixed versions, multiple runs and runtime boundaries.
mod support;
use runtime::ownership::{
    CheckpointBoundary as Boundary, Lease, OwnerCheckpoint, OwnerPermit, Registry,
    SnapshotNoneReason as Reason, SnapshotReference as Reference, ThreadOwner,
};

fn fixture() -> (tempfile::TempDir, OwnerPermit) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    Registry::open(&path).unwrap().start(&owner).unwrap();
    (
        directory,
        OwnerPermit {
            registry_path: path,
            thread_id: owner.thread_id,
            lease: owner.lease,
            run_id: Some("parent".into()),
        },
    )
}
fn payload(permit: &OwnerPermit) -> String {
    rusqlite::Connection::open(&permit.registry_path)
        .unwrap()
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id='thread'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}
fn row(permit: &OwnerPermit) -> OwnerCheckpoint {
    OwnerCheckpoint::decode(&payload(permit)).unwrap()
}

#[test]
fn metadata_multiple_active_runs_terminal_refresh_and_pruning() {
    let (_directory, parent) = fixture();
    let mut child = parent.clone();
    child.run_id = Some("child".into());
    parent.begin_turn().unwrap();
    child.begin_turn().unwrap();
    let before_idle = Registry::open_existing(&parent.registry_path)
        .unwrap()
        .attach("thread")
        .unwrap();
    assert!(
        parent
            .checkpoint_metadata(Boundary::Idle, Reference::none(Reason::Skipped))
            .is_err()
    );
    assert_eq!(
        Registry::open_existing(&parent.registry_path)
            .unwrap()
            .attach("thread")
            .unwrap(),
        before_idle
    );
    let receipt = storage::RunContextReceipt {
        updated_at_ns: 123,
        messages_end: 12,
        sha256: "ab".repeat(32),
    };
    parent
        .checkpoint_metadata(
            Boundary::TurnEnd,
            Reference::RunContext {
                receipt: receipt.clone(),
            },
        )
        .unwrap();
    assert!(
        Registry::open_existing(&parent.registry_path)
            .unwrap()
            .attach("thread")
            .unwrap()
            .active_turn
    );
    assert_eq!(
        row(&parent).runs["parent"].snapshot,
        Reference::RunContext { receipt }
    );
    let before_idle = payload(&parent);
    assert!(
        parent
            .checkpoint_metadata(Boundary::Idle, Reference::none(Reason::Skipped))
            .is_err(),
        "a run already drained must not report idle while its child is active"
    );
    assert_eq!(payload(&parent), before_idle);
    parent.begin_turn().unwrap();
    child
        .checkpoint_metadata(Boundary::TurnEnd, Reference::none(Reason::WriteFailed))
        .unwrap();
    let both = row(&parent);
    assert_eq!(both.runs.len(), 2);
    assert!(both.runs.contains_key("parent"));
    assert_eq!(
        both.runs["child"].snapshot,
        Reference::none(Reason::WriteFailed)
    );
    let mut idle = parent.clone();
    idle.run_id = None;
    assert!(idle.checkpoint(&[]).is_err());
    parent
        .checkpoint_metadata(Boundary::TurnEnd, Reference::none(Reason::NoRunStore))
        .unwrap();
    assert_eq!(row(&parent).runs.len(), 1);
    parent
        .checkpoint_metadata(Boundary::Terminal, Reference::none(Reason::NoRunStore))
        .unwrap();
    assert_eq!(row(&parent).runs["parent"].boundary, Boundary::Terminal);
    idle.checkpoint(&[]).unwrap();
    let mut registry = Registry::open_existing(&parent.registry_path).unwrap();
    registry
        .update("thread", |owner| {
            owner.quiesce(&parent.lease)?;
            owner.release(&parent.lease)
        })
        .unwrap();
    let before = payload(&parent);
    for boundary in [Boundary::Terminal, Boundary::TurnEnd, Boundary::Idle] {
        assert!(
            parent
                .checkpoint_metadata(boundary, Reference::none(Reason::Skipped))
                .is_err()
        );
    }
    assert_eq!(payload(&parent), before);
}

#[test]
fn legacy_arrays_unknown_versions_and_old_positional_writes_remain_compatible() {
    let (_directory, permit) = fixture();
    let connection = rusqlite::Connection::open(&permit.registry_path).unwrap();
    for old in [
        "[]",
        "[{\"secret\":\"legacy-body\"}]",
        "{\"v\":999}",
        "broken",
    ] {
        assert!(OwnerCheckpoint::decode(old).is_none());
        connection
            .execute(
                "INSERT OR REPLACE INTO owner_checkpoints VALUES ('thread','1',?1)",
                [old],
            )
            .unwrap();
        permit.begin_turn().unwrap();
        permit
            .checkpoint_metadata(Boundary::TurnEnd, Reference::none(Reason::Skipped))
            .unwrap();
        assert_eq!(row(&permit).generation, 1);
        assert!(!payload(&permit).contains("legacy-body"));
    }
    connection
        .execute(
            "INSERT OR REPLACE INTO owner_checkpoints VALUES ('thread','1','[]')",
            [],
        )
        .unwrap();
    // Recovery authority never consults either the legacy payload or its version.
    Registry::open_existing(&permit.registry_path)
        .unwrap()
        .update("thread", |owner| {
            owner.lease.expires_at = 0;
            owner.claim(&permit.lease, "successor", u64::MAX / 2, 0)
        })
        .unwrap();
    assert!(
        permit
            .checkpoint_metadata(Boundary::Terminal, Reference::none(Reason::Skipped))
            .is_err()
    );
}

#[test]
fn a_five_mb_body_never_enters_owner_db_or_journal_and_size_is_stable() {
    let (_directory, mut permit) = fixture();
    permit.run_id = None;
    let secret = "unredacted-owner-checkpoint-secret";
    let messages = [providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: format!("{secret}{}", "x".repeat(5_000_000)),
        }],
    }];
    for _ in 0..1000 {
        permit.checkpoint(&messages).unwrap();
    }
    assert!(payload(&permit).len() < 1024);
    assert!(std::fs::metadata(&permit.registry_path).unwrap().len() < 65536);
    let bytes = std::fs::read(&permit.registry_path).unwrap();
    assert!(
        !bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    let journal = permit.registry_path.with_extension("db-journal");
    if journal.exists() {
        let bytes = std::fs::read(journal).unwrap();
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
    }
}

#[tokio::test]
async fn runtime_continues_with_none_for_missing_failed_and_suspended_stores() {
    use event_bus::{AgentRunPhase, EventBus};
    use runtime::{AgentRuntime, Role, RunConfig, RunStore};
    use std::sync::Arc;
    for mode in [
        "absent",
        "trigger",
        "suspended",
        "written",
        "terminal-owner-failure",
    ] {
        let (directory, mut permit) = fixture();
        permit.run_id = None;
        let bus = Arc::new(EventBus::new(128));
        let model = Arc::new(support::ScriptedModel::new([Ok(support::text_response(
            "answer",
            providers::FinishReason::Stop,
        ))]));
        let executor = Arc::new(tools::ToolExecutor::new(bus.clone()));
        let mut runtime = AgentRuntime::new(bus, executor, model);
        let mut config = storage::StorageConfig {
            db_path: directory.path().join("events.db"),
            ..Default::default()
        };
        if mode == "suspended" {
            config.hard_limits.max_db_bytes = 1;
        }
        let store = storage::Storage::open(config.clone()).unwrap();
        if mode == "trigger" {
            rusqlite::Connection::open(&config.db_path).unwrap().execute_batch("CREATE TRIGGER fail_snapshot BEFORE UPDATE ON run_contexts BEGIN SELECT RAISE(ABORT, 'test snapshot failure'); END;").unwrap();
        }
        if mode == "terminal-owner-failure" {
            rusqlite::Connection::open(&permit.registry_path).unwrap().execute_batch("CREATE TRIGGER fail_terminal BEFORE UPDATE ON owner_checkpoints WHEN json_extract(NEW.payload, '$.boundary')='terminal' BEGIN SELECT RAISE(ABORT, 'test owner failure'); END;").unwrap();
        }
        if mode != "absent" {
            runtime = runtime.with_run_store(RunStore::open(&config, store.handle()).unwrap());
        }
        let run = runtime.delegate_background(
            Role::Worker,
            "prompt".into(),
            RunConfig {
                ownership: Some(permit.clone()),
                ..Default::default()
            },
        );
        let phase = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.wait(run))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            phase,
            if mode == "terminal-owner-failure" || mode == "suspended" {
                AgentRunPhase::Error
            } else {
                AgentRunPhase::Done
            },
            "{mode}"
        );
        let checkpoint = row(&permit);
        let snapshot = &checkpoint.runs[&run.to_string()].snapshot;
        match mode {
            "absent" => assert_eq!(*snapshot, Reference::none(Reason::NoRunStore)),
            "trigger" | "suspended" => assert_eq!(*snapshot, Reference::none(Reason::WriteFailed)),
            "written" => {
                let saved = storage::Database::open(&config)
                    .unwrap()
                    .run_context(&run.to_string())
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.terminal_phase, "Done");
                assert!(snapshot.available(Some(&saved)));
                assert_eq!(checkpoint.boundary, Boundary::Terminal);
            }
            "terminal-owner-failure" => {
                let saved = storage::Database::open(&config)
                    .unwrap()
                    .run_context(&run.to_string())
                    .unwrap()
                    .unwrap();
                assert_eq!(saved.terminal_phase, "Error");
                assert_eq!(checkpoint.boundary, Boundary::TurnEnd);
                assert!(!snapshot.available(Some(&saved)));
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(unix)]
#[test]
fn quiesce_during_snapshot_preparation_waits_for_reference_commit_before_handoff() {
    use runtime::ownership::ipc::{Request, Response, request, serve_connection};
    use std::sync::mpsc;

    let (directory, old) = fixture();
    old.begin_turn().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let handle = store.handle();
    let snapshot_permit = old.clone();
    let (prepared_tx, prepared_rx) = mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = mpsc::sync_channel(0);
    let snapshot = std::thread::spawn(move || {
        snapshot_permit.validate_generation().unwrap();
        prepared_tx.send(()).unwrap();
        resume_rx.recv().unwrap();
        handle
            .upsert_run_context_receipt(&storage::RunContextRecord {
                run_id: "parent".into(),
                role: "Worker".into(),
                name: "Worker".into(),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "Done".into(),
                restorable: false,
                updated_at_ns: 1,
            })
            .unwrap()
    });
    prepared_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();

    let socket = directory.path().join("owner.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let registry_path = old.registry_path.clone();
    let server = std::thread::spawn(move || {
        let mut registry = Registry::open_existing(&registry_path).unwrap();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            serve_connection(&mut stream, &mut registry, 20).unwrap();
        }
    });
    assert!(matches!(
        request(
            &socket,
            &Request::Quiesce {
                thread_id: old.thread_id.clone(),
                token: old.lease.clone(),
            }
        )
        .unwrap(),
        Response::Status(owner) if owner.active_turn
    ));
    resume_tx.send(()).unwrap();
    let reference = Reference::RunContext {
        receipt: snapshot.join().unwrap(),
    };
    let saved = storage::Database::open(&config)
        .unwrap()
        .run_context("parent")
        .unwrap()
        .unwrap();
    assert!(reference.available(Some(&saved)));
    let handoff = Request::Handoff {
        thread_id: old.thread_id.clone(),
        token: old.lease.clone(),
        successor_id: "successor".into(),
        settings: config::OwnershipConfig::default(),
    };
    assert!(
        matches!(request(&socket, &handoff).unwrap(), Response::Rejected(_)),
        "durable history alone must not release the active run"
    );
    old.checkpoint_metadata(Boundary::Terminal, reference)
        .unwrap();
    assert!(row(&old).runs["parent"].snapshot.available(Some(&saved)));
    assert!(matches!(
        request(&socket, &handoff).unwrap(),
        Response::Status(owner)
            if owner.lease.generation == 2 && owner.active_runs.is_empty()
    ));
    assert!(
        old.checkpoint_metadata(Boundary::Terminal, Reference::none(Reason::Skipped))
            .is_err()
    );
    server.join().unwrap();
}

#[test]
fn delayed_old_generation_snapshot_can_replace_history_but_cannot_commit_authority() {
    let (directory, old) = fixture();
    old.begin_turn().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let mut record = storage::RunContextRecord {
        run_id: "parent".into(),
        role: "Worker".into(),
        name: "Worker".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: "[]".into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Checkpoint".into(),
        restorable: false,
        updated_at_ns: 1,
    };
    // The old process passed preflight and wrote history, then paused before
    // committing the reference. No read guard spans events DB I/O.
    old.validate_generation().unwrap();
    let old_receipt = store.handle().upsert_run_context_receipt(&record).unwrap();
    let mut registry = Registry::open_existing(&old.registry_path).unwrap();
    let owner = registry
        .update("thread", |owner| {
            owner.lease.expires_at = 0;
            owner.claim(&old.lease, "successor", u64::MAX / 2, 0)
        })
        .unwrap();
    let current = OwnerPermit {
        lease: owner.lease,
        ..old.clone()
    };
    current.begin_turn().unwrap();
    record.updated_at_ns = 2;
    record.messages_json = r#"[{"content":"successor"}]"#.into();
    let current_receipt = store.handle().upsert_run_context_receipt(&record).unwrap();
    current
        .checkpoint_metadata(
            Boundary::TurnEnd,
            Reference::RunContext {
                receipt: current_receipt,
            },
        )
        .unwrap();
    current.begin_turn().unwrap();
    let authority_before = registry.attach("thread").unwrap();
    let metadata_before = payload(&current);
    assert!(
        old.checkpoint_metadata(
            Boundary::Terminal,
            Reference::RunContext {
                receipt: old_receipt
            }
        )
        .is_err()
    );
    assert_eq!(registry.attach("thread").unwrap(), authority_before);
    assert_eq!(payload(&current), metadata_before);
    // Existing last-writer-wins limitation (Q8), not a newly promised fence.
    record.updated_at_ns = 1;
    record.messages_json = r#"[{"content":"late old snapshot"}]"#.into();
    store.handle().upsert_run_context(&record).unwrap();
    let saved = storage::Database::open(&config)
        .unwrap()
        .run_context("parent")
        .unwrap()
        .unwrap();
    assert!(saved.messages_json.contains("late old snapshot"));
    assert!(
        !row(&current).runs["parent"]
            .snapshot
            .available(Some(&saved))
    );
    assert!(!row(&current).runs["parent"].snapshot.available(None));
    assert_eq!(registry.attach("thread").unwrap(), authority_before);
    registry
        .update("thread", |owner| owner.quiesce(&current.lease))
        .unwrap();
    current
        .checkpoint_metadata(Boundary::Terminal, Reference::none(Reason::WriteFailed))
        .unwrap();
    registry
        .update("thread", |owner| owner.release(&current.lease))
        .unwrap();
}
