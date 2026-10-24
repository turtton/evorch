//! Thread rows written by the runtime and the host, and the thread-scoped reads.

use storage::threads::{ThreadCursor, ThreadLineageKind, ThreadSource, ThreadUpsert};
use storage::{Database, RunContextRecord, Storage, StorageConfig, StorageError};

fn fixture() -> (tempfile::TempDir, StorageConfig, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("threads.sqlite3"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    (dir, config, storage)
}

fn secret() -> String {
    format!("sk-{}", "abcdefghijklmnopqrstuvwxyz0123456789")
}

fn thread(id: &str, project_root: &str, updated_at_ns: i64) -> ThreadUpsert {
    ThreadUpsert {
        thread_id: id.into(),
        project_root: Some(project_root.into()),
        created_at_ns: 1,
        updated_at_ns,
        ..Default::default()
    }
}

fn context(run_id: &str, parent: Option<&str>, config: &str, messages: &str) -> RunContextRecord {
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: format!("name-{run_id}"),
        parent_run_id: parent.map(str::to_owned),
        config_json: config.into(),
        messages_json: messages.into(),
        checkpoints_json: "[]".into(),
        terminal_phase: "Done".into(),
        restorable: false,
        updated_at_ns: 1,
    }
}

#[test]
fn upserts_are_idempotent_and_runtime_never_overwrites_host_values() {
    // Given: a thread the runtime recorded, including escalation lineage.
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    let runtime = ThreadUpsert {
        thread_id: "t".into(),
        project_root: Some("/runtime-root".into()),
        root_run_id: Some("run-1".into()),
        chat_role: Some("Orchestrator".into()),
        lineage_kind: Some(ThreadLineageKind::Escalation),
        escalation_source_run_id: Some("run-0".into()),
        created_at_ns: 10,
        updated_at_ns: 10,
        ..Default::default()
    };
    handle.upsert_thread_runtime(&runtime).unwrap();
    let database = Database::open(&config).unwrap();
    let first = database.thread("t").unwrap().unwrap();
    assert_eq!(first.source, ThreadSource::Runtime);
    assert!(!first.archived && !first.superseded);

    // When: the same write is repeated.
    handle.upsert_thread_runtime(&runtime).unwrap();
    // Then: nothing changes.
    assert_eq!(database.thread("t").unwrap().unwrap(), first);

    // When: the host mirrors the thread without knowing its lineage.
    let host = ThreadUpsert {
        thread_id: "t".into(),
        project_id: Some("project".into()),
        project_root: Some("/canonical".into()),
        title: Some("Fix login".into()),
        root_run_id: Some("run-2".into()),
        archived: Some(true),
        superseded: Some(false),
        created_at_ns: 5,
        updated_at_ns: 20,
        ..Default::default()
    };
    handle
        .upsert_threads_host(std::slice::from_ref(&host))
        .unwrap();
    // Then: host values replace the runtime's, and runtime-only lineage is kept.
    let mirrored = database.thread("t").unwrap().unwrap();
    assert_eq!(mirrored.source, ThreadSource::Host);
    assert_eq!(
        (
            mirrored.project_id.as_deref(),
            mirrored.project_root.as_deref(),
            mirrored.title.as_deref(),
            mirrored.root_run_id.as_deref(),
            mirrored.archived,
            mirrored.created_at_ns,
            mirrored.updated_at_ns,
        ),
        (
            Some("project"),
            Some("/canonical"),
            Some("Fix login"),
            Some("run-2"),
            true,
            5,
            20
        )
    );
    assert_eq!(
        (
            mirrored.lineage_kind,
            mirrored.escalation_source_run_id.as_deref(),
            mirrored.chat_role.as_deref(),
        ),
        (
            Some(ThreadLineageKind::Escalation),
            Some("run-0"),
            Some("Orchestrator")
        )
    );

    // When: the runtime writes conflicting values and one the host left empty.
    handle
        .upsert_thread_runtime(&ThreadUpsert {
            project_root: Some("/elsewhere".into()),
            title: Some("runtime title".into()),
            root_run_id: Some("run-3".into()),
            archived: Some(false),
            fork_point_run_id: Some("run-1".into()),
            fork_point_context_len: Some(4),
            created_at_ns: 1,
            updated_at_ns: 15,
            ..runtime.clone()
        })
        .unwrap();
    // Then: only the empty columns are filled, and activity never moves back.
    let after = database.thread("t").unwrap().unwrap();
    assert_eq!(
        (
            after.project_root.as_deref(),
            after.title.as_deref(),
            after.root_run_id.as_deref(),
            after.archived,
            after.created_at_ns,
            after.updated_at_ns,
            after.source,
        ),
        (
            Some("/canonical"),
            Some("Fix login"),
            Some("run-2"),
            true,
            5,
            20,
            ThreadSource::Host
        )
    );
    assert_eq!(
        (
            after.fork_point_run_id.as_deref(),
            after.fork_point_context_len
        ),
        (Some("run-1"), Some(4))
    );

    // When: the host repeats its mirror.
    handle.upsert_threads_host(&[host]).unwrap();
    // Then: the row is unchanged.
    assert_eq!(database.thread("t").unwrap().unwrap(), after);
}

#[test]
fn runtime_replaces_its_own_values_but_none_never_erases() {
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    handle
        .upsert_thread_runtime(&ThreadUpsert {
            root_run_id: Some("run-1".into()),
            chat_role: Some("Worker".into()),
            ..thread("t", "/repo", 1)
        })
        .unwrap();

    handle
        .upsert_thread_runtime(&ThreadUpsert {
            thread_id: "t".into(),
            root_run_id: Some("run-2".into()),
            created_at_ns: 9,
            updated_at_ns: 2,
            ..Default::default()
        })
        .unwrap();

    let row = Database::open(&config)
        .unwrap()
        .thread("t")
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            row.project_root.as_deref(),
            row.root_run_id.as_deref(),
            row.chat_role.as_deref(),
            row.created_at_ns,
            row.updated_at_ns,
        ),
        (Some("/repo"), Some("run-2"), Some("Worker"), 1, 2)
    );
}

#[test]
fn host_batches_redact_titles_and_reject_empty_ids_atomically() {
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    let titled = ThreadUpsert {
        title: Some(format!("deploy with {}", secret())),
        ..thread("titled", "/repo", 1)
    };

    let error = handle
        .upsert_threads_host(&[titled.clone(), thread("", "/repo", 1)])
        .unwrap_err();
    assert_eq!(
        error,
        StorageError::Serialization("thread id is empty".into())
    );
    let database = Database::open(&config).unwrap();
    assert!(database.thread("titled").unwrap().is_none());

    handle.upsert_threads_host(&[titled]).unwrap();
    let title = database.thread("titled").unwrap().unwrap().title.unwrap();
    assert!(!title.contains(&secret()), "{title}");
    assert!(title.starts_with("deploy with [REDACTED:"), "{title}");
}

#[test]
fn try_upsert_threads_queues_without_waiting() {
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();

    assert!(handle.try_upsert_threads(vec![thread("queued", "/repo", 3)]));
    assert!(handle.try_upsert_threads(Vec::new()));
    // A later blocking request is served after the queued mirror.
    handle.flush_usage_now().unwrap();

    let row = Database::open(&config)
        .unwrap()
        .thread("queued")
        .unwrap()
        .unwrap();
    assert_eq!((row.source, row.updated_at_ns), (ThreadSource::Host, 3));
}

#[test]
fn project_pages_follow_the_cursor_without_gaps_or_repeats() {
    // Given: threads of one project with an activity tie, and others outside it.
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    handle
        .upsert_threads_host(&[
            thread("a", "/repo", 10),
            thread("b", "/repo", 20),
            thread("c", "/repo", 20),
            thread("d", "/repo", 30),
            thread("e", "/repo", 40),
            thread("outside", "/repo/sub", 50),
            ThreadUpsert {
                project_root: None,
                ..thread("unknown", "", 60)
            },
        ])
        .unwrap();
    let database = Database::open_read_only(&config.db_path).unwrap();

    // When: the project is read two threads at a time through encoded cursors.
    let mut pages = Vec::new();
    let mut cursor: Option<ThreadCursor> = None;
    loop {
        let page = database
            .threads_in_project("/repo", cursor.as_ref(), 2)
            .unwrap();
        pages.push(
            page.threads
                .iter()
                .map(|thread| thread.thread_id.clone())
                .collect::<Vec<_>>(),
        );
        match page.next_cursor {
            Some(next) => cursor = ThreadCursor::decode(&next.encode()),
            None => break,
        }
    }

    // Then: newest first, ties by ID descending, each thread exactly once.
    assert_eq!(pages, [vec!["e", "d"], vec!["c", "b"], vec!["a"]]);
    // And: a zero limit still returns one thread.
    let first = database.threads_in_project("/repo", None, 0).unwrap();
    assert_eq!(first.threads.len(), 1);
    assert!(first.next_cursor.is_some());
}

#[test]
fn saved_contexts_feed_thread_reads_with_stored_redactions() {
    // Given: a conversation root with an explicit thread, a delegated child,
    // notes, and a final answer that contained a secret.
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    let final_messages = format!(
        r#"[{{"role":"system","content":[{{"type":"text","text":"rules"}}]}},
            {{"role":"user","content":[{{"type":"text","text":"go"}}]}},
            {{"role":"assistant","content":[{{"type":"reasoning","text":"hidden"}},
              {{"type":"text","text":"done with {}"}}]}}]"#,
        secret()
    );
    handle
        .upsert_run_context(&context(
            "root",
            None,
            r#"{"thread_id":"t","non_restorable_reason":"ownership"}"#,
            &final_messages,
        ))
        .unwrap();
    handle
        .upsert_run_context(&context(
            "child",
            Some("root"),
            "{}",
            r#"[{"role":"user","content":[{"type":"text","text":"task"}]}]"#,
        ))
        .unwrap();
    handle.append_run_ledger("root", "first").unwrap();
    handle.append_run_ledger("child", "").unwrap();
    handle.append_run_ledger("root", "second").unwrap();
    let database = Database::open_read_only(&config.db_path).unwrap();

    // When / Then: both runs belong to the thread.
    let runs = database.runs_of_thread("t").unwrap();
    assert_eq!(
        runs.iter()
            .map(|run| (run.run_id.as_str(), run.root_run_id.as_str(), run.depth))
            .collect::<Vec<_>>(),
        [("root", "root", 0), ("child", "root", 1)]
    );
    assert_eq!(
        database.thread_of_run("child").unwrap().unwrap().thread_id,
        "t"
    );

    // And: contexts come back in the requested order, once, skipping unsaved runs.
    let ids = |values: &[&str]| values.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    let contexts = database
        .run_contexts_for(&ids(&["child", "missing", "root", "child"]))
        .unwrap();
    assert_eq!(
        contexts
            .iter()
            .map(|record| record.run_id.as_str())
            .collect::<Vec<_>>(),
        ["child", "root"]
    );
    assert!(!contexts[1].messages_json.contains(&secret()));

    // And: only the latest note per run is returned, explicit clears included.
    let notes = database
        .latest_ledgers(&ids(&["root", "child", "none"]))
        .unwrap();
    assert_eq!(
        notes
            .iter()
            .map(|note| (note.run_id.as_str(), note.body.as_str()))
            .collect::<Vec<_>>(),
        [("child", ""), ("root", "second")]
    );

    // And: summaries expose the redacted final answer without reasoning.
    let summaries = database
        .terminal_summaries(&ids(&["root", "child", "root"]))
        .unwrap();
    assert_eq!(summaries.len(), 2);
    let root = &summaries[0];
    assert_eq!(
        (
            root.run_id.as_str(),
            root.terminal_phase.as_str(),
            root.message_count,
            root.non_restorable_reason.as_deref(),
        ),
        ("root", "Done", 3, Some("ownership"))
    );
    let final_text = root.final_text.as_deref().unwrap();
    assert!(
        final_text.starts_with("done with [REDACTED:"),
        "{final_text}"
    );
    assert!(!final_text.contains("hidden"));
    let child = &summaries[1];
    assert_eq!(
        (
            child.parent_run_id.as_deref(),
            child.message_count,
            child.final_text.as_deref()
        ),
        (Some("root"), 1, None)
    );
}
