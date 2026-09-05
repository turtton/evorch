//! ADR 0008 defense-in-depth — 公開書き込み経路の heuristic secret guard を検証します。
//! 本 guard は完全な secret 非漏洩保証ではなく、検出は deterministic な規則のみに
//! 依存します。

use std::time::{Duration, UNIX_EPOCH};

use event_bus::{
    Event, EventKind, EventMeta, GoalState, LifecycleEvent, MessageEvent, OrchestratorEvent,
    ProviderEvent, ToolEvent,
};
use storage::{Database, Storage, StorageConfig, StorageError};
use tempfile::TempDir;

const KNOWN_ENV: &str = "GH_TOKEN";
const KNOWN_SENTINEL: &str = "evorch-it-known-sentinel-58901bcd-0123456789";

fn config(temp: &TempDir) -> StorageConfig {
    StorageConfig {
        db_path: temp.path().join("guard.db"),
        ..StorageConfig::default()
    }
}

fn event(kind: EventKind) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::ZERO,
            wall_clock: UNIX_EPOCH + Duration::from_secs(60),
        },
        kind,
    }
}

fn message_delta(text: &str) -> Event {
    event(
        MessageEvent::MessageDelta {
            delta: text.to_owned(),
        }
        .into(),
    )
}

#[test]
fn handle_rejects_representative_api_key_shapes_without_persisting_anything() {
    // Given: 代表的な credential 形状を env 名含む本文へ混入させたイベント
    let cases: [(&str, &str); 9] = [
        ("sk-test-evorch-9f8e7d6c5b4a3f2e1d", "openai-style-key"),
        ("sk-ant-api03-aaaa-bbbb-cccc-dddd-eeee", "openai-style-key"),
        ("ghp_0123456789abcdefghijklmnopqrstuvwxyz", "github-token"),
        ("github_pat_11ABCDEFGH_ijklmnopqrstuvwxyz", "github-pat"),
        ("xoxb-1234-5678-abcdefgh", "slack-token"),
        ("AKIAIOSFODNN7EXAMPLE", "aws-access-key-id"),
        ("AIzaSyAbcdefghijklmnopqrstuvwxyz01234567", "google-api-key"),
        (
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c",
            "jwt",
        ),
        ("-----BEGIN OPENSSH PRIVATE KEY-----", "private-key-block"),
    ];

    for (token, rule) in cases {
        let temp = TempDir::new().expect("temporary directory must be created");
        let storage = Storage::open(config(&temp)).expect("storage must open");
        let handle = storage.handle();

        // When: 形状を含む delta を公開経路で追記する
        let Err(error) = handle.append_event(Some("s"), &message_delta(&format!("leak: {token}")))
        else {
            panic!("token with rule {rule} must be rejected");
        };

        // Then: 規則ラベル付きで拒否され、診断に値本体も前後コンテキストも含まれない
        assert!(matches!(
            error,
            StorageError::SecretDetected {
                entity: "event",
                field: "MessageDelta.delta",
                ..
            }
        ));
        let rendered = format!("{} / {:?}", error, error);
        assert!(
            !rendered.contains(token),
            "diagnostic leaked the secret value"
        );
        assert!(
            !rendered.contains("leak: "),
            "diagnostic leaked the context"
        );
        assert!(rendered.contains(rule));
        assert!(rendered.contains("heuristic"));
        storage.close();

        // And: events テーブルは空のまま
        let db = Database::open(&config(&temp)).expect("database must open");
        assert!(db.events_all_ordered().unwrap().is_empty());
    }
}

#[test]
fn handle_rejects_known_credential_env_value_without_persisting_anything() {
    // Given: 限定 credential env 名に注入された既知値
    let previous = std::env::var(KNOWN_ENV).ok();
    // SAFETY: テストプロセス内で一意の sentinel のみを設定し、終了時に復元する。
    // 本ファイル内の他テストの fixture は sentinel を含まず、並行実行と競合しない。
    unsafe { std::env::set_var(KNOWN_ENV, KNOWN_SENTINEL) };
    let temp = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp)).expect("storage must open");
    let handle = storage.handle();

    // When: 既知値を含む delta を追記する
    let result = handle.append_event(
        Some("s"),
        &message_delta(&format!("key is {KNOWN_SENTINEL}")),
    );
    let display = result.as_ref().err().map(|error| error.to_string());
    drop(storage);
    let db = Database::open(&config(&temp)).expect("database must open");
    let rows = db.events_all_ordered().unwrap().len();
    // SAFETY: 上記 sentinel の後始末であり、外部環境へ影響を残さない。
    unsafe {
        match &previous {
            Some(value) => std::env::set_var(KNOWN_ENV, value),
            None => std::env::remove_var(KNOWN_ENV),
        }
    }

    // Then: known-credential-value 規則で拒否され、値は診断に含まれず、行は増えない
    let Err(error) = result else {
        panic!("known credential value must be rejected");
    };
    assert!(matches!(
        error,
        StorageError::SecretDetected {
            entity: "event",
            field: "MessageDelta.delta",
            ..
        }
    ));
    let display = display.expect("rejection display must exist");
    assert!(display.contains("rule=known-credential-value"));
    assert!(!display.contains(KNOWN_SENTINEL));
    assert_eq!(rows, 0);
}

#[test]
fn rejected_event_preserves_events_rows_and_session_bytes() {
    // Given: セッション開始イベントが保存済みで projection が行を構成済みの状態
    let temp = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp)).expect("storage must open");
    let handle = storage.handle();
    handle
        .append_event(
            Some("s"),
            &event(
                LifecycleEvent::Started {
                    session_id: "s".into(),
                }
                .into(),
            ),
        )
        .expect("started event must be accepted");
    handle.reconcile().expect("reconcile must succeed");
    let db = Database::open(&config(&temp)).expect("database must open");
    let bytes_before = db
        .session("s")
        .unwrap()
        .expect("session must exist")
        .total_event_bytes;
    let rows_before = db.events_all_ordered().unwrap().len();

    // When: reason 系 field へ secret 形状値を含むイベントを複数種類追記する
    let secret = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";
    let rejected = [
        event(
            LifecycleEvent::Failed {
                session_id: "s".into(),
                reason: format!("died {secret}"),
            }
            .into(),
        ),
        event(
            ProviderEvent::ProviderFallback {
                from_provider: "a".into(),
                to_provider: "b".into(),
                reason: format!("flip {secret}"),
            }
            .into(),
        ),
        event(
            ToolEvent::ExecutionDenied {
                tool_name: "read".into(),
                call_id: "c".into(),
                reason: format!("deny {secret}"),
            }
            .into(),
        ),
        event(
            ProviderEvent::RequestCompleted {
                request_id: "req-1".into(),
                provider: "p".into(),
                profile: None,
                protocol: "proto".into(),
                model: "m".into(),
                streaming: true,
                duration_ms: 10,
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
                finish_reason: format!("other {secret}"),
                run_id: None,
            }
            .into(),
        ),
    ];
    for bad in &rejected {
        if handle.append_event(Some("s"), bad).is_ok() {
            panic!("reason field containing a secret must be rejected");
        }
    }

    // Then: events 行数と session bytes は不変
    handle.reconcile().expect("reconcile must succeed");
    storage.close();
    let bytes_after = db
        .session("s")
        .unwrap()
        .expect("session must exist")
        .total_event_bytes;
    let rows_after = db.events_all_ordered().unwrap().len();
    assert_eq!(rows_after, rows_before);
    assert_eq!(bytes_after, bytes_before);
}

#[test]
fn orchestrator_event_payload_is_secret_checked() {
    // Given: goal 本文に secret 形状値を含む Orchestrator::GoalCreated イベント
    let temp = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp)).expect("storage must open");
    let handle = storage.handle();
    let secret = "sk-test-evorch-9f8e7d6c5b4a3f2e1d";

    // When: 公開経路で追記する
    let result = handle.append_event(
        Some("s"),
        &event(
            OrchestratorEvent::GoalCreated {
                goal_id: "goal-1".into(),
                session_id: "s".into(),
                project_id: "evorch".into(),
                thread_id: "t".into(),
                goal: format!("implement {secret}"),
                references: Vec::new(),
                constraints: Vec::new(),
                repo: "turtton/evorch".into(),
                base_ref: "main".into(),
                root_run_id: "run-1".into(),
            }
            .into(),
        ),
    );

    // Then: payload 全体の走査で拒否され、診断に値本体が含まれない
    let Err(error) = result else {
        panic!("orchestrator payload containing a secret must be rejected");
    };
    assert!(matches!(
        error,
        StorageError::SecretDetected {
            entity: "event",
            field: "Orchestrator.payload",
            ..
        }
    ));
    let rendered = format!("{error:?}");
    assert!(
        !rendered.contains(secret),
        "diagnostic leaked the secret value"
    );
    storage.close();

    // And: events テーブルは空のまま
    let db = Database::open(&config(&temp)).expect("database must open");
    assert!(db.events_all_ordered().unwrap().is_empty());

    // And: secret を含まない Orchestrator イベントは受理されて永続化される
    let storage = Storage::open(config(&temp)).expect("storage must open");
    let handle = storage.handle();
    handle
        .append_event(
            Some("s"),
            &event(
                OrchestratorEvent::GoalStateChanged {
                    goal_id: "goal-1".into(),
                    from: GoalState::Active,
                    to: GoalState::Paused,
                    reason: "operator pause".into(),
                }
                .into(),
            ),
        )
        .expect("clean orchestrator event must be accepted");
    storage.close();
    let db = Database::open(&config(&temp)).expect("database must open");
    assert_eq!(db.events_all_ordered().unwrap().len(), 1);
}

#[test]
fn normal_prose_and_short_token_like_values_are_not_rejected() {
    // Given: 通常文と credential に見えるが規則を満たさない文字列群
    let temp = TempDir::new().expect("temporary directory must be created");
    let storage = Storage::open(config(&temp)).expect("storage must open");
    let handle = storage.handle();
    let corpus = [
        "hello, this is a normal message",
        "これは通常の日本語の文章です。環境変数 OPENAI_API_KEY を設定してください。",
        "abc12345",
        "ghp_short",
        "sk-x",
        "ask-this-boundary-must-not-trip-the-guard-0123456789abcdef",
        "123e4567-e89b-12d3-a456-426614174000",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4",
        "a fragment eyJhbGciOiJIUzI1NiIs without dot segments",
        "-----BEGIN CERTIFICATE-----\nMIIB",
    ];

    // When / Then: すべて永続化に成功する
    for text in corpus {
        handle
            .append_event(Some("s"), &message_delta(text))
            .expect("negative corpus must be accepted");
    }
    storage.close();
    let db = Database::open(&config(&temp)).expect("database must open");
    assert_eq!(db.events_all_ordered().unwrap().len(), corpus.len());
}
