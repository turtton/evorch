//! The `subkind`/`run_id` columns: what ingress writes, what the backfill
//! converts, and that every index-driven read agrees with a plain scan whether
//! or not rows are converted.

use event_bus::{Event, EventKind};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::select::{
    IDS_BY_KIND_SQL, IDS_BY_RUN_SQL, IDS_BY_SUBKIND_SQL, IDS_UNCONVERTED_BY_KIND_SQL,
    UNCONVERTED_EXISTS_SQL,
};
use super::*;
use crate::inspect::EventScope;
use crate::migrations::backfill::{BATCH_SELECT_SQL, EventBackfillOptions};
use crate::{Database, StorageConfig};

fn open() -> Database {
    Database::open_in_memory().unwrap()
}

fn event_with(kind: EventKind) -> Event {
    Event {
        meta: event_bus::EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_nanos(1),
            wall_clock: std::time::UNIX_EPOCH + Duration::from_secs(10),
        },
        kind,
    }
}

fn kind_from(value: Value) -> EventKind {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error}: {value}"))
}

fn adjacent(category: &str, variant: &str, payload: Value) -> Value {
    json!({"kind": category, "payload": {"kind": variant, "payload": payload}})
}

fn append(conn: &Connection, kind: &EventKind) -> i64 {
    append_event(
        conn,
        None,
        &event_with(kind.clone()),
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}

/// Stores the event as an earlier version would have: without the new columns.
fn insert_unconverted(conn: &Connection, kind: &EventKind) -> i64 {
    conn.execute(
        "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (NULL, 1, 1, 10000000000, ?1, ?2)",
        params![kind_name(kind), serde_json::to_string(kind).unwrap()],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn stored_columns(conn: &Connection, id: i64) -> (String, Option<String>, Option<String>) {
    conn.query_row(
        "SELECT kind, subkind, run_id FROM events WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

/// One event of every persisted variant: category tag, variant, payload and the
/// run it must be attributed to. The expectations are written out, not derived.
#[allow(clippy::too_many_lines)]
fn every_variant() -> Vec<(&'static str, &'static str, Value, Option<&'static str>)> {
    let question = json!({
        "id": "q1", "run_id": "run-child", "root_run_id": "run-root", "root_name": "root",
        "recipient_run_ids": ["run-root"], "title": "t", "options": [], "blocking": true,
        "answer": null
    });
    let request = |extra: Value| {
        let mut base = json!({
            "request_id": "req", "provider": "p", "profile": null, "protocol": "x",
            "model": "m", "streaming": true
        });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    };
    let goal = json!({
        "goal_id": "g", "thread_id": "t", "root_run_id": "run-root",
        "related_root_run_ids": [], "objective": "o", "criteria": [], "checks": [],
        "phase": "working", "review_enabled": false, "checks_paused": false,
        "work_stopped": false, "epoch": 1, "review_round": 0, "findings": [],
        "reason": null, "usage": {"model_requests": 0, "input_tokens": 0, "output_tokens": 0},
        "max_review_rounds": 1, "max_tokens": null, "original_request": "r"
    });
    vec![
        (
            "Lifecycle",
            "RunProgress",
            json!({"run_id": "run-a", "activity": "Model", "context": null}),
            Some("run-a"),
        ),
        ("Lifecycle", "Started", json!({"session_id": "s"}), None),
        (
            "Lifecycle",
            "Delegated",
            json!({"session_id": "s", "target": "t"}),
            None,
        ),
        (
            "Lifecycle",
            "BackgroundTaskStarted",
            json!({"task_id": "t"}),
            None,
        ),
        (
            "Lifecycle",
            "BackgroundTaskCompleted",
            json!({"task_id": "t"}),
            None,
        ),
        (
            "Lifecycle",
            "BackgroundTaskCancelled",
            json!({"task_id": "t"}),
            None,
        ),
        (
            "Lifecycle",
            "AgentRunStarted",
            json!({"run_id": "run-a", "parent_run_id": "run-parent", "agent_name": "n", "role": "r"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "TaskPromptPublished",
            json!({"run_id": "run-a", "parent_run_id": null, "agent_name": "n", "role": "r", "prompt": "p"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "AgentRunStateChanged",
            json!({"run_id": "run-a", "from": "Running", "to": "Done", "reason": null}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "TurnCompleted",
            json!({"run_id": "run-a", "context_len": 1}),
            Some("run-a"),
        ),
        ("Lifecycle", "Completed", json!({"session_id": "s"}), None),
        (
            "Lifecycle",
            "Failed",
            json!({"session_id": "s", "reason": "r"}),
            None,
        ),
        (
            "Lifecycle",
            "RoutingDecision",
            json!({"shape": "s", "reason": "r", "source": {"kind": "LocalRule", "payload": {"rule": "x"}}}),
            None,
        ),
        (
            "Lifecycle",
            "EscalationRequested",
            json!({"source_run_id": "run-a", "new_run_id": "run-b", "summary": {"original_request": "o", "escalation_reason": "e", "files_touched": [], "blockers": [], "suggested_next": "n"}}),
            None,
        ),
        (
            "Lifecycle",
            "EscalationProposed",
            json!({"run_id": "run-a", "trigger": {"tool_call_threshold": {"count": 3}}}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "AgentRunRestored",
            json!({"run_id": "run-a", "restored_by": "u", "message_id": "m"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "RootWakeRequested",
            json!({"run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "AutoContinuationPaused",
            json!({"run_id": "run-a", "limit": 3}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "HostInputDelivered",
            json!({"run_id": "run-a", "prompt_id": "p"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "HostInputDropped",
            json!({"run_id": "run-a", "prompt_id": "p"}),
            Some("run-a"),
        ),
        (
            "Lifecycle",
            "HostInputUnknown",
            json!({"run_id": "run-a", "prompt_id": "p"}),
            Some("run-a"),
        ),
        (
            "Message",
            "MessageCompleted",
            json!({"run_id": "run-a", "text": "t"}),
            Some("run-a"),
        ),
        (
            "Message",
            "FinalResultPublished",
            json!({"run_id": "run-a", "text": "t"}),
            Some("run-a"),
        ),
        (
            "Message",
            "MessageDelta",
            json!({"delta": "d", "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Message",
            "MessageDelta",
            json!({"delta": "d", "run_id": null}),
            None,
        ),
        (
            "Message",
            "ReasoningDelta",
            json!({"delta": "d", "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Message",
            "ToolCallDelta",
            json!({"run_id": "run-a", "index": 0, "call_id": null, "tool_name": null, "arguments_delta": "a"}),
            Some("run-a"),
        ),
        ("Message", "StreamRetried", json!({"run_id": null}), None),
        (
            "Tool",
            "UserQuestionUpdated",
            json!({"question": question}),
            Some("run-child"),
        ),
        (
            "Tool",
            "ArtifactsPresented",
            json!({"run_id": "run-a", "presentation": {"presentation_id": "p", "artifacts": []}}),
            Some("run-a"),
        ),
        (
            "Tool",
            "ToolStarted",
            json!({"tool_name": "t", "call_id": "run-b:c", "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Tool",
            "ToolStarted",
            json!({"tool_name": "t", "call_id": "run-b:c", "run_id": null}),
            None,
        ),
        (
            "Tool",
            "ToolCompleted",
            json!({"tool_name": "t", "call_id": "c", "is_error": false, "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Tool",
            "ShellJobOutput",
            json!({"job_id": "j", "run_id": "run-a", "offset": 0, "chunk": "c", "status": "running"}),
            Some("run-a"),
        ),
        (
            "Tool",
            "ApprovalRequested",
            json!({"tool_name": "t", "call_id": "run-a:c"}),
            None,
        ),
        (
            "Tool",
            "ApprovalResolved",
            json!({"call_id": "run-a:c", "approved": true}),
            None,
        ),
        (
            "Tool",
            "ExecutionDenied",
            json!({"tool_name": "t", "call_id": "run-a:c", "reason": "r"}),
            None,
        ),
        (
            "Provider",
            "ProviderFallback",
            json!({"from_provider": "a", "to_provider": "b", "reason": "r"}),
            None,
        ),
        (
            "Provider",
            "RequestStarted",
            request(json!({"run_id": "run-a"})),
            Some("run-a"),
        ),
        (
            "Provider",
            "FirstTokenObserved",
            request(json!({"ttft_ms": 1, "run_id": "run-a"})),
            Some("run-a"),
        ),
        (
            "Provider",
            "RequestCompleted",
            request(
                json!({"duration_ms": 1, "input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0, "cache_write_tokens": 0, "finish_reason": "stop", "run_id": "run-a"}),
            ),
            Some("run-a"),
        ),
        (
            "Provider",
            "CacheReuseObserved",
            json!({"request_id": "r", "cache_read_tokens": 1, "comparison": {"kind": "NoBaseline", "reason": "Expired"}, "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Provider",
            "RequestFailed",
            request(json!({"duration_ms": 1, "failure": {"kind": "Timeout"}, "run_id": null})),
            None,
        ),
        (
            "Provider",
            "CooldownStarted",
            json!({"profile": "p", "model": null, "failure": {"kind": "Quota"}, "cooldown_ms": null, "run_id": "run-a"}),
            Some("run-a"),
        ),
        (
            "Provider",
            "FallbackTriggered",
            json!({"from_provider": "a", "to_provider": "b", "to_model": "m", "logical_model": "l", "session_id": "run-a", "failure": {"kind": "Auth"}, "request_id": null}),
            Some("run-a"),
        ),
        (
            "Fault",
            "SubscriberLagged",
            json!({"subscriber_id": 1, "skipped": 2}),
            None,
        ),
        (
            "Fault",
            "SkillDiagnostic",
            json!({"kind": "shadowed", "skill": "s", "scope": "c", "detail": "d"}),
            None,
        ),
        (
            "AgentMessage",
            "Delivered",
            json!({"message": {"message_id": "m", "sender_run_id": "run-s", "recipient_run_id": "run-r", "kind": "send", "content": "c", "reply_to": null}, "disposition": "steering"}),
            Some("run-r"),
        ),
        (
            "Compaction",
            "Compacted",
            json!({"run_id": "run-a", "reason": "automatic", "threshold": 0.8, "context_window_tokens": 1, "estimated_tokens_before": 2, "estimated_tokens_after": 1, "compacted_range_start": 0, "compacted_range_end": 1, "checkpoint_id": "c", "summary": "s"}),
            Some("run-a"),
        ),
        (
            "Orchestrator",
            "ThreadGoalUpdated",
            json!({"snapshot": goal}),
            None,
        ),
        (
            "Orchestrator",
            "ThreadTodoUpdated",
            json!({"snapshot": {"list_id": "l", "thread_id": "t", "revision": 1, "items": []}}),
            None,
        ),
        (
            "Orchestrator",
            "TaskProgressed",
            json!({"task_id": "t", "run_id": "run-a", "progress": {}, "reason": "r"}),
            None,
        ),
        (
            "Orchestrator",
            "TaskCheckpoint",
            json!({"task_id": "t", "run_id": "run-a", "tool_call_count": 1, "cumulative_input_tokens": 1, "cumulative_output_tokens": 1, "elapsed_ms": 1}),
            None,
        ),
        (
            "Ledger",
            "RunLedgerAppended",
            json!({"run_id": "run-a", "seq": 1, "body": "b"}),
            Some("run-a"),
        ),
    ]
}

#[test]
fn every_variant_is_stored_with_the_columns_its_helpers_and_payload_give() {
    // Given: one event of every persisted variant plus the two without an inner tag.
    let database = open();
    let mut cases: Vec<(EventKind, &str, Option<&str>)> = every_variant()
        .into_iter()
        .map(|(category, variant, payload, run)| {
            (
                kind_from(adjacent(category, variant, payload)),
                variant,
                run,
            )
        })
        .collect();
    cases.push((
        kind_from(json!({"kind": "Diagnostic", "payload": {
            "source": "s", "severity": "info", "code": "c", "detail": "d",
            "run_id": "run-a", "thread_id": null
        }})),
        "",
        Some("run-a"),
    ));
    cases.push((
        kind_from(json!({"kind": "Diagnostic", "payload": {
            "source": "s", "severity": "info", "code": "c", "detail": "d",
            "run_id": null, "thread_id": null
        }})),
        "",
        None,
    ));
    cases.push((
        kind_from(json!({"kind": "Ownership", "payload": {
            "thread_id": "t", "owner_id": "o", "generation": 1, "action": "claimed"
        }})),
        "",
        None,
    ));

    for (kind, variant, run) in &cases {
        // When: it is appended.
        let id = append(&database.conn, kind);

        // Then: the columns equal the helpers' answers, the expected values, and
        // the tag the payload itself carries.
        let (stored_kind, subkind, run_id) = stored_columns(&database.conn, id);
        assert_eq!(stored_kind, kind_name(kind));
        assert_eq!(subkind.as_deref(), Some(kind.subkind_name()), "{variant}");
        assert_eq!(subkind.as_deref(), Some(*variant), "{variant}");
        assert_eq!(run_id.as_deref(), kind.primary_run_id(), "{variant}");
        assert_eq!(run_id.as_deref(), *run, "{variant}");
        let payload: Value = serde_json::from_str(
            &database
                .conn
                .query_row("SELECT payload FROM events WHERE id = ?1", [id], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            payload["payload"]["kind"].as_str().unwrap_or(""),
            kind.subkind_name(),
            "{variant}"
        );
    }
    assert_eq!(cases.len(), 57);
}

#[test]
fn usage_events_have_helper_values_but_are_never_stored() {
    // Given: usage events, which are accounted in the ledger instead of events.
    for (variant, payload) in [
        (
            "Usage",
            json!({"provider": "p", "model": "m", "input_tokens": 1, "output_tokens": 1, "cache_read_tokens": 0, "cache_write_tokens": 0}),
        ),
        (
            "CacheStats",
            json!({"provider": "p", "model": "m", "cache_hits": 1, "cache_misses": 0}),
        ),
    ] {
        let kind = kind_from(adjacent("Usage", variant, payload));

        // Then: the helpers still name them, and ingress refuses them.
        assert_eq!(kind.subkind_name(), variant);
        assert_eq!(kind.primary_run_id(), None);
        let database = open();
        assert!(matches!(
            append_event(
                &database.conn,
                None,
                &event_with(kind),
                &HardLimits::default(),
                &mut EventAccounting::default()
            ),
            Err(StorageError::RawUsageEventNotPersisted)
        ));
    }
}

fn apply_migrations_up_to(conn: &Connection, version: usize) {
    for (index, migration) in crate::migrations::MIGRATIONS[..version].iter().enumerate() {
        conn.execute_batch(migration).unwrap();
        conn.pragma_update(None, "user_version", index + 1).unwrap();
    }
}

fn legacy_kinds() -> Vec<EventKind> {
    every_variant()
        .into_iter()
        .filter(|(category, ..)| *category != "Usage")
        .map(|(category, variant, payload, _)| kind_from(adjacent(category, variant, payload)))
        .collect()
}

#[test]
fn migration_leaves_old_rows_unconverted_and_backfill_fills_them_resumably() {
    // Given: a database at the version before the columns, with decodable rows
    // and one whose variant no longer exists.
    let conn = Connection::open_in_memory().unwrap();
    apply_migrations_up_to(&conn, 29);
    let kinds = legacy_kinds();
    for kind in &kinds {
        conn.execute(
            "INSERT INTO events (schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (1, 1, 1, ?1, ?2)",
            params![kind_name(kind), serde_json::to_string(kind).unwrap()],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO events (schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (1, 1, 1, 'Lifecycle', '{\"kind\":\"Lifecycle\",\"payload\":{\"kind\":\"Removed\",\"payload\":{}}}')",
        [],
    )
    .unwrap();
    let total = kinds.len() as u64 + 1;

    // When: the current migrations are applied.
    crate::migrations::apply_migrations(&conn).unwrap();
    let database = Database { conn };

    // Then: no row has been converted yet, and opening did not convert any.
    assert_eq!(database.unbackfilled_event_count().unwrap(), total);

    // When: the backfill is interrupted after a few rows and then resumed.
    let first = database
        .backfill_event_columns(&EventBackfillOptions {
            batch_rows: 4,
            max_rows: Some(10),
        })
        .unwrap();
    assert_eq!((first.converted, first.remaining), (10, total - 10));
    assert_eq!(first.batches, 3);
    let rest = database
        .backfill_event_columns(&EventBackfillOptions {
            batch_rows: 4,
            max_rows: None,
        })
        .unwrap();

    // Then: everything is converted exactly once and the removed variant is blank.
    assert_eq!(
        (rest.converted, rest.remaining, rest.undecodable),
        (total - 10, 0, 1)
    );
    let again = database
        .backfill_event_columns(&EventBackfillOptions::default())
        .unwrap();
    assert_eq!((again.converted, again.batches), (0, 0));
    let mut statement = database
        .conn
        .prepare("SELECT id, kind, subkind, run_id, payload FROM events ORDER BY id")
        .unwrap();
    let rows: Vec<(i64, String, String, Option<String>, String)> = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len() as u64, total);
    for (kind, (_, stored_kind, subkind, run_id, _)) in kinds.iter().zip(&rows) {
        assert_eq!(stored_kind, kind_name(kind));
        assert_eq!(subkind, kind.subkind_name());
        assert_eq!(run_id.as_deref(), kind.primary_run_id());
    }
    let (_, _, subkind, run_id, _) = rows.last().unwrap();
    assert_eq!((subkind.as_str(), run_id.as_deref()), ("", None));
}

fn plan(conn: &Connection, sql: &str, bound: usize) -> String {
    let params: Vec<&dyn rusqlite::ToSql> = vec![&"x" as &dyn rusqlite::ToSql; bound];
    let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows: Vec<String> = statement
        .query_map(params.as_slice(), |row| row.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.join(" | ")
}

#[test]
fn lookups_use_the_new_indexes() {
    let database = open();
    let conn = &database.conn;
    for (sql, bound, index) in [
        (IDS_BY_KIND_SQL, 1, "idx_events_kind_subkind_id"),
        (IDS_BY_SUBKIND_SQL, 2, "idx_events_kind_subkind_id"),
        (IDS_UNCONVERTED_BY_KIND_SQL, 1, "idx_events_kind_subkind_id"),
        (IDS_BY_RUN_SQL, 1, "idx_events_run_id"),
        (UNCONVERTED_EXISTS_SQL, 0, "idx_events_unbackfilled"),
        (BATCH_SELECT_SQL, 2, "idx_events_unbackfilled"),
    ] {
        let plan = plan(conn, sql, bound);
        assert!(plan.contains(index), "{sql}: {plan}");
        assert!(
            plan.split(" | ")
                .filter(|step| step.contains(" events"))
                .all(|step| step.contains("INDEX")),
            "{sql}: {plan}"
        );
    }
}

/// Events by index in the scenario below. Odd/even placement alternates between
/// rows written by this version and rows an earlier version left unconverted.
struct Scenario {
    database: Database,
    ids: Vec<i64>,
}

fn scenario() -> Scenario {
    let database = open();
    let message = |run: &str, text: &str| {
        kind_from(adjacent(
            "Message",
            "MessageCompleted",
            json!({"run_id": run, "text": text}),
        ))
    };
    let approval = |subkind: &str, payload: Value| kind_from(adjacent("Tool", subkind, payload));
    let agent = |sender: &str, recipient: &str| {
        kind_from(adjacent(
            "AgentMessage",
            "Delivered",
            json!({"message": {"message_id": "m", "sender_run_id": sender, "recipient_run_id": recipient, "kind": "send", "content": "c", "reply_to": null}, "disposition": "steering"}),
        ))
    };
    let events: Vec<(bool, EventKind)> = vec![
        (
            false,
            kind_from(adjacent(
                "Lifecycle",
                "AgentRunStarted",
                json!({"run_id": "run-1", "parent_run_id": null, "agent_name": "n", "role": "r"}),
            )),
        ),
        (true, message("run-1", "one")),
        (false, message("run-2", "two")),
        (
            true,
            approval(
                "ApprovalRequested",
                json!({"tool_name": "t", "call_id": "run-1:call-1"}),
            ),
        ),
        (
            false,
            approval(
                "ApprovalResolved",
                json!({"call_id": "run-1:call-1", "approved": true}),
            ),
        ),
        (
            true,
            approval(
                "ApprovalRequested",
                json!({"tool_name": "t", "call_id": "run-2:call-1"}),
            ),
        ),
        (false, agent("run-2", "run-3")),
        (true, agent("run-1", "run-2")),
        (
            false,
            kind_from(adjacent(
                "Orchestrator",
                "ThreadTodoUpdated",
                json!({"snapshot": {"list_id": "l", "thread_id": "t", "revision": 1, "items": []}}),
            )),
        ),
        (
            true,
            kind_from(adjacent(
                "Orchestrator",
                "TaskProgressed",
                json!({"task_id": "t", "run_id": "run-1", "progress": {}, "reason": "r"}),
            )),
        ),
        (
            false,
            kind_from(adjacent(
                "Orchestrator",
                "TaskCheckpoint",
                json!({"task_id": "t", "run_id": "run-9", "tool_call_count": 1, "cumulative_input_tokens": 1, "cumulative_output_tokens": 1, "elapsed_ms": 1}),
            )),
        ),
        (true, message("run-10", "ten")),
        (false, message("run-2", "\"run_id\":\"run-1\"")),
        (
            true,
            kind_from(
                json!({"kind": "Ownership", "payload": {"thread_id": "t", "owner_id": "o", "generation": 1, "action": "claimed"}}),
            ),
        ),
        (
            true,
            kind_from(
                json!({"kind": "Diagnostic", "payload": {"source": "s", "severity": "info", "code": "c", "detail": "d", "run_id": "run-1", "thread_id": null}}),
            ),
        ),
        (
            false,
            kind_from(
                json!({"kind": "Diagnostic", "payload": {"source": "s", "severity": "info", "code": "c", "detail": "d", "run_id": null, "thread_id": null}}),
            ),
        ),
    ];
    let ids = events
        .iter()
        .map(|(converted, kind)| {
            if *converted {
                append(&database.conn, kind)
            } else {
                insert_unconverted(&database.conn, kind)
            }
        })
        .collect();
    Scenario { database, ids }
}

impl Scenario {
    fn pick(&self, indexes: &[usize]) -> Vec<i64> {
        indexes.iter().map(|&index| self.ids[index]).collect()
    }

    fn matching(&self, filters: &[(&str, Option<&str>)]) -> Vec<i64> {
        let mut ids = Vec::new();
        self.database
            .for_each_event_matching(filters, |stored| ids.push(stored.id))
            .unwrap();
        ids
    }

    fn for_runs(&self, runs: &[&str]) -> Vec<i64> {
        let runs: Vec<String> = runs.iter().map(|run| (*run).to_owned()).collect();
        let mut ids = Vec::new();
        self.database
            .for_each_event_for_runs(&runs, |stored| ids.push(stored.id))
            .unwrap();
        ids
    }

    fn inspected(&self, run: &str) -> Vec<i64> {
        self.database
            .inspect_events(EventScope::Run(run), 100)
            .unwrap()
            .0
            .iter()
            .map(|event| event.id)
            .collect()
    }

    fn check_reads(&self) {
        // Filtering by kind alone and by kind and subkind, across both row kinds.
        assert_eq!(
            self.matching(&[("Orchestrator", None)]),
            self.pick(&[8, 9, 10])
        );
        assert_eq!(
            self.matching(&[("Orchestrator", Some("TaskProgressed"))]),
            self.pick(&[9])
        );
        assert_eq!(
            self.matching(&[
                ("Orchestrator", Some("ThreadTodoUpdated")),
                ("Message", Some("MessageCompleted")),
                ("Message", None),
            ]),
            self.pick(&[1, 2, 8, 11, 12])
        );
        assert_eq!(self.matching(&[("Tool", None)]), self.pick(&[3, 4, 5]));
        assert_eq!(
            self.matching(&[("Tool", Some("ApprovalResolved")), ("Ownership", Some(""))]),
            self.pick(&[4, 13])
        );
        assert_eq!(
            self.matching(&[("Message", Some("StreamRetried"))]),
            Vec::<i64>::new()
        );
        assert_eq!(
            self.database.orchestrator_events_ordered().unwrap().len(),
            3
        );

        // A run's own events, agent messages in both directions, approvals by
        // call ID; never another run's event that merely mentions it.
        assert_eq!(self.for_runs(&["run-1"]), self.pick(&[0, 1, 3, 4, 7, 14]));
        assert_eq!(self.for_runs(&["run-2"]), self.pick(&[2, 5, 6, 7, 12]));
        assert_eq!(self.for_runs(&["run-3"]), self.pick(&[6]));
        assert_eq!(
            self.for_runs(&["run-2", "run-1"]),
            self.pick(&[0, 1, 2, 3, 4, 5, 6, 7, 12, 14])
        );
        assert_eq!(self.for_runs(&["run-404"]), Vec::<i64>::new());
        let (listed, more) = self
            .database
            .events_for_runs(&["run-1".to_owned()], 0, 3)
            .unwrap();
        assert_eq!(
            listed.iter().map(|stored| stored.id).collect::<Vec<_>>(),
            self.pick(&[0, 1, 3])
        );
        assert!(more);
        let (after, _) = self
            .database
            .events_for_runs(&["run-1".to_owned()], 20_000_000_000, 10)
            .unwrap();
        assert!(after.is_empty());

        // Inspection also finds the run named inside orchestrator task events.
        assert_eq!(self.inspected("run-1"), self.pick(&[0, 1, 9, 14]));
        assert_eq!(self.inspected("run-2"), self.pick(&[2, 7, 12]));
        let (limited, truncated) = self
            .database
            .inspect_events(EventScope::Run("run-1"), 2)
            .unwrap();
        assert_eq!(
            limited.iter().map(|event| event.id).collect::<Vec<_>>(),
            self.pick(&[9, 14])
        );
        assert!(truncated);
    }
}

#[test]
fn reads_agree_before_during_and_after_the_backfill() {
    // Given: a history mixing converted and unconverted rows.
    let scenario = scenario();
    assert_eq!(scenario.database.unbackfilled_event_count().unwrap(), 8);

    // Then: every read already answers correctly through the fallback.
    scenario.check_reads();

    // When/Then: it keeps answering the same while the backfill progresses.
    scenario
        .database
        .backfill_event_columns(&EventBackfillOptions {
            batch_rows: 2,
            max_rows: Some(4),
        })
        .unwrap();
    assert_eq!(scenario.database.unbackfilled_event_count().unwrap(), 4);
    scenario.check_reads();

    // When/Then: and once nothing is left.
    scenario
        .database
        .backfill_event_columns(&EventBackfillOptions::default())
        .unwrap();
    assert_eq!(scenario.database.unbackfilled_event_count().unwrap(), 0);
    scenario.check_reads();
}

#[test]
fn undecodable_unconverted_rows_are_skipped_by_index_reads() {
    // Given: a row written by an older build whose variant is gone.
    let database = open();
    let kept = append(
        &database.conn,
        &kind_from(adjacent(
            "Message",
            "MessageCompleted",
            json!({"run_id": "run-1", "text": "t"}),
        )),
    );
    database
        .conn
        .execute(
            "INSERT INTO events (schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (1, 1, 1, 'Message', '{\"kind\":\"Message\",\"payload\":{\"kind\":\"Gone\",\"payload\":{\"run_id\":\"run-1\"}}}')",
            [],
        )
        .unwrap();

    // When/Then: reads return only the decodable event, before and after backfill.
    for _ in 0..2 {
        let mut ids = Vec::new();
        database
            .for_each_event_matching(&[("Message", None)], |stored| ids.push(stored.id))
            .unwrap();
        assert_eq!(ids, [kept]);
        let mut ids = Vec::new();
        database
            .for_each_event_for_runs(&["run-1".to_owned()], |stored| ids.push(stored.id))
            .unwrap();
        assert_eq!(ids, [kept]);
        database
            .backfill_event_columns(&EventBackfillOptions::default())
            .unwrap();
    }
}

#[test]
fn read_only_handle_serves_the_same_reads() {
    // Given: a file database that is reopened read-only.
    let dir = tempfile::TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        ..StorageConfig::default()
    };
    let database = Database::open(&config).unwrap();
    let id = append(
        &database.conn,
        &kind_from(adjacent(
            "Message",
            "MessageCompleted",
            json!({"run_id": "run-1", "text": "t"}),
        )),
    );
    drop(database);
    let read_only = Database::open_read_only(&config.db_path).unwrap();

    // Then: index reads work without write access.
    let mut ids = Vec::new();
    read_only
        .for_each_event_for_runs(&["run-1".to_owned()], |stored| ids.push(stored.id))
        .unwrap();
    assert_eq!(ids, [id]);
}
