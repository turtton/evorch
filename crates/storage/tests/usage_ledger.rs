//! Usage ledger: backfill from provider events, attribution joins and retention rollup.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_bus::{
    Event, EventMeta, LifecycleEvent, ProviderEvent, ProviderFailureKind, RequestPurpose,
};
use rusqlite::Connection;
use storage::usage::{RunAttribution, UsageRequestRecord, UsageStatus};
use storage::{Database, Storage, StorageConfig, system_time_to_ns};
use tempfile::TempDir;

fn config(temp_dir: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp_dir.path().join("evorch.db"),
        ..StorageConfig::default()
    }
}

fn event(kind: impl Into<event_bus::EventKind>, seconds: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(seconds),
            wall_clock: UNIX_EPOCH + Duration::from_secs(1_700_000_000 + seconds),
        },
        kind: kind.into(),
    }
}

fn completed(request_id: &str, run_id: Option<&str>) -> ProviderEvent {
    ProviderEvent::RequestCompleted {
        request_id: request_id.into(),
        provider: "openai".into(),
        profile: Some("main".into()),
        protocol: "openai-chat-completions".into(),
        model: "gpt-test".into(),
        streaming: true,
        duration_ms: 900,
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 60,
        cache_write_tokens: 5,
        finish_reason: "stop".into(),
        service_tier: None,
        run_id: run_id.map(Into::into),
        reasoning_tokens: Some(7),
        purpose: Some(RequestPurpose::Compaction),
    }
}

fn record(request_id: &str, at: SystemTime, cost_usd: Option<f64>) -> UsageRequestRecord {
    UsageRequestRecord {
        request_id: request_id.into(),
        at_ns: system_time_to_ns(at).unwrap(),
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "gpt-test".into(),
        run_id: Some("run-1".into()),
        parent_run_id: None,
        role: Some("worker".into()),
        purpose: Some("agent".into()),
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: Some("stop".into()),
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 60,
        cache_write_tokens: 5,
        reasoning_tokens: None,
        ttft_ms: Some(300),
        duration_ms: 900,
        cost_usd,
        service_tier: None,
    }
}

/// Persist events through the writer, then rewind the schema to v13 so the
/// next open replays the v14 backfill over them.
fn v13_database_with_events(config: &StorageConfig, events: &[Event]) {
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    for event in events {
        handle.append_event(Some("session"), event).unwrap();
    }
    storage.close();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(
        "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
         DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
         DROP TABLE threads; DROP TABLE thread_runs;
         DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
         ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
         DROP TABLE eval_samples; DROP TABLE thread_start_untracked; DROP TABLE thread_start_states;
         DROP TABLE assistant_references; DROP TABLE review_evidence; DROP TABLE review_lineage_edges;
         DROP TABLE usage_requests; DROP TABLE usage_run_threads; DROP TABLE usage_daily;
         DROP INDEX idx_memory_entries_scope_status;
         ALTER TABLE memory_entries DROP COLUMN scope; ALTER TABLE memory_ledger DROP COLUMN scope;
         ALTER TABLE improvement_candidates DROP COLUMN occurrences;
         ALTER TABLE improvement_candidates DROP COLUMN last_seen_at_ns;
         ALTER TABLE improvement_candidates DROP COLUMN recent_run_ids;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 13).unwrap();
}

#[test]
fn v14_backfills_requests_from_persisted_provider_events() {
    // Given: a v13 database holding a subagent request with TTFT, a failed
    // entry-routing request and a request without a run.
    let temp_dir = TempDir::new().unwrap();
    let config = config(&temp_dir);
    v13_database_with_events(
        &config,
        &[
            event(
                LifecycleEvent::AgentRunStarted {
                    run_id: "run-2".into(),
                    parent_run_id: Some("run-1".into()),
                    agent_name: "explorer".into(),
                    role: "explorer".into(),
                },
                1,
            ),
            event(
                ProviderEvent::FirstTokenObserved {
                    request_id: "req-a".into(),
                    provider: "openai".into(),
                    profile: Some("main".into()),
                    protocol: "openai-chat-completions".into(),
                    model: "gpt-test".into(),
                    ttft_ms: 250,
                    run_id: Some("run-2".into()),
                },
                2,
            ),
            event(completed("req-a", Some("run-2")), 3),
            event(
                ProviderEvent::RequestFailed {
                    detail: None,
                    request_id: "req-b".into(),
                    provider: "openai".into(),
                    profile: None,
                    protocol: "openai-chat-completions".into(),
                    model: "gpt-test".into(),
                    streaming: false,
                    duration_ms: 40,
                    failure: ProviderFailureKind::Http { status: 503 },
                    run_id: Some("entry-routing".into()),
                    purpose: None,
                },
                4,
            ),
            event(completed("req-c", None), 5),
        ],
    );

    // When: the database is opened by the current schema.
    let database = Database::open(&config).unwrap();
    let rows = database.usage_requests_between(0, i64::MAX).unwrap();

    // Then: every terminal attempt becomes one row with what legacy events know.
    let ids: Vec<_> = rows
        .iter()
        .map(|row| row.record.request_id.as_str())
        .collect();
    assert_eq!(ids, ["req-a", "req-b", "req-c"]);
    let subagent = &rows[0].record;
    assert_eq!(subagent.ttft_ms, Some(250));
    assert_eq!(subagent.role.as_deref(), Some("explorer"));
    assert_eq!(subagent.parent_run_id.as_deref(), Some("run-1"));
    assert_eq!(subagent.purpose.as_deref(), Some("agent"));
    assert_eq!(
        (
            subagent.input_tokens,
            subagent.cache_read_tokens,
            subagent.cache_write_tokens
        ),
        (100, 60, 5)
    );
    assert_eq!(subagent.cost_usd, None);
    assert_eq!(subagent.reasoning_tokens, None);
    let routing = &rows[1].record;
    assert_eq!(routing.status, UsageStatus::Failed);
    assert_eq!(routing.failure.as_deref(), Some("Http:503"));
    assert_eq!(routing.purpose.as_deref(), Some("routing"));
    assert_eq!(rows[2].record.purpose, None);
}

#[test]
fn recorded_requests_join_attribution_bound_later() {
    // Given: a request recorded before its run is bound to a thread.
    let temp_dir = TempDir::new().unwrap();
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let now = SystemTime::now();
    handle
        .record_usage_requests(vec![record("req-1", now, Some(0.25))])
        .unwrap();

    // When: the owner arrives afterwards and the same request is re-delivered.
    assert!(handle.try_attribute_usage_runs(vec![RunAttribution {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        project_id: Some("project-1".into()),
    }]));
    let mut duplicate = record("req-1", now, Some(9.0));
    duplicate.output_tokens = 999;
    handle.record_usage_requests(vec![duplicate]).unwrap();
    storage.close();

    // Then: the original row is kept and reads carry the late attribution.
    let rows = Database::open(&config)
        .unwrap()
        .usage_requests_between(0, i64::MAX)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record, record("req-1", now, Some(0.25)));
    assert_eq!(rows[0].thread_id.as_deref(), Some("thread-1"));
    assert_eq!(rows[0].project_id.as_deref(), Some("project-1"));
}

#[test]
fn maintenance_rolls_expired_requests_into_daily_totals() {
    // Given: two expired requests (one without a recorded price) and a recent one.
    let temp_dir = TempDir::new().unwrap();
    let config = StorageConfig {
        usage_retention_days: 30,
        ..config(&temp_dir)
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let now = SystemTime::now();
    let expired = now - Duration::from_secs(40 * 86_400);
    assert!(handle.try_attribute_usage_runs(vec![RunAttribution {
        run_id: "run-1".into(),
        thread_id: "thread-1".into(),
        project_id: Some("project-1".into()),
    }]));
    handle
        .record_usage_requests(vec![
            record("old-priced", expired, Some(0.5)),
            record("old-unpriced", expired, None),
            record("recent", now, Some(0.1)),
        ])
        .unwrap();

    // When: a maintenance tick runs.
    handle.checkpoint_now().unwrap();
    storage.close();

    // Then: expired rows move to one daily bucket and the recent row stays.
    let database = Database::open(&config).unwrap();
    let remaining = database.usage_requests_between(0, i64::MAX).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].record.request_id, "recent");
    let daily = database
        .usage_daily_between("0000-01-01", "9999-12-31")
        .unwrap();
    assert_eq!(daily.len(), 1);
    let day = &daily[0];
    assert_eq!(day.project_id.as_deref(), Some("project-1"));
    assert_eq!(day.role.as_deref(), Some("worker"));
    assert_eq!(day.purpose.as_deref(), Some("agent"));
    assert_eq!((day.request_count, day.failed_count), (2, 0));
    assert_eq!(day.input_tokens, 200);
    assert_eq!(day.cost_usd, 0.5);
    assert_eq!(
        (
            day.unpriced_input_tokens,
            day.unpriced_output_tokens,
            day.unpriced_cache_read_tokens,
            day.unpriced_cache_write_tokens
        ),
        (100, 20, 60, 5)
    );
    assert_eq!((day.ttft_sum_ms, day.ttft_count), (600, 2));
}

#[test]
fn local_day_reads_bound_requests_by_the_ledger_clock() {
    // Given: a request now and one three days earlier.
    let temp_dir = TempDir::new().unwrap();
    let config = config(&temp_dir);
    let storage = Storage::open(config.clone()).unwrap();
    let now = SystemTime::now();
    storage
        .handle()
        .record_usage_requests(vec![
            record("today", now, Some(0.1)),
            record("earlier", now - Duration::from_secs(3 * 86_400), Some(0.1)),
        ])
        .unwrap();
    storage.close();
    let database = Database::open(&config).unwrap();

    // When: only today's local day is read.
    let today = database.usage_local_today().unwrap();
    let rows = database.usage_requests_in_days(&today, &today).unwrap();

    // Then: the row is labelled with that day and older rows are excluded.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record.request_id, "today");
    assert_eq!(rows[0].day, today);
    assert_eq!(rows[0].time.len(), "HH:MM:SS".len());
    let clock = database.usage_local_clock().unwrap();
    assert_eq!(clock.today, today);
    assert!(clock.day_start_ns <= rows[0].record.at_ns && rows[0].record.at_ns < clock.day_end_ns);
}

#[test]
fn v19_records_service_tier_and_keeps_earlier_requests_standard() {
    // Given: a v18 database holding a request recorded before service tiers existed.
    let temp_dir = TempDir::new().unwrap();
    let config = config(&temp_dir);
    let now = SystemTime::now();
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .record_usage_requests(vec![record("earlier", now, Some(0.25))])
        .unwrap();
    storage.close();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(
        "ALTER TABLE review_evidence DROP COLUMN attachments_json; DROP INDEX host_input_receipts_seq; ALTER TABLE host_input_receipts DROP COLUMN acceptance_seq; ALTER TABLE host_input_receipts DROP COLUMN evidence_id; DROP INDEX idx_events_kind_subkind_id; DROP INDEX idx_events_run_id; DROP INDEX idx_events_unbackfilled; DROP INDEX idx_events_blank_subkind; ALTER TABLE events DROP COLUMN subkind; ALTER TABLE events DROP COLUMN run_id; CREATE INDEX idx_events_session_id ON events(session_id, id); DROP TABLE learning_claim_roots; DROP TABLE learning_claims; DROP TABLE run_context_provenance; DROP TABLE thread_index_runs; DROP TABLE conversation_fts; DROP TABLE conversation_blocks; DROP TABLE thread_index_state; DROP TABLE thread_index_lease; DROP TABLE thread_index_requests; DROP TABLE thread_index_request_targets; ALTER TABLE run_contexts DROP COLUMN snapshot_revision;
         DROP TABLE host_input_receipts; ALTER TABLE run_contexts DROP COLUMN host_inputs_json;
         DROP TABLE threads; DROP TABLE thread_runs;
         DROP INDEX idx_run_contexts_parent; DROP INDEX idx_usage_requests_parent;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_count;
         ALTER TABLE improvement_candidates DROP COLUMN reopened_at_ns;
         ALTER TABLE tasks ADD COLUMN resume_cursor_json TEXT;
         DROP TABLE eval_samples; DROP TABLE thread_start_untracked; DROP TABLE thread_start_states;
         ALTER TABLE usage_requests DROP COLUMN service_tier;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 18).unwrap();
    drop(conn);

    // When: opening applies v19 and a fast request is recorded.
    let storage = Storage::open(config.clone()).unwrap();
    let fast = UsageRequestRecord {
        service_tier: Some("priority".into()),
        ..record("fast", now, Some(1.0))
    };
    storage
        .handle()
        .record_usage_requests(vec![fast.clone()])
        .unwrap();
    storage.close();

    // Then: the earlier request reads as standard and the fast one keeps its tier.
    let rows = Database::open(&config)
        .unwrap()
        .usage_requests_between(0, i64::MAX)
        .unwrap();
    let records: Vec<_> = rows.into_iter().map(|row| row.record).collect();
    assert_eq!(records, [record("earlier", now, Some(0.25)), fast]);
}
