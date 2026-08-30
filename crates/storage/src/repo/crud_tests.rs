//! 永続化エンティティの CRUD 統合テスト。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::entity::{
    AgentRunRecord, AgentRunStatus, MessageRecord, MessageRole, SessionRecord, SessionStatus,
    TaskRecord, TaskStatus,
};
use crate::repo::{agent_run, message, session, task};
use crate::{Database, StorageConfig, StorageError};
use rusqlite::Connection;
use tempfile::TempDir;

fn open_connection(temp_dir: &TempDir) -> Connection {
    let path = temp_dir.path().join("storage.db");
    let config = StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("database must open"));
    let connection = Connection::open(path).expect("database must reopen");
    connection
        .pragma_update(None, "foreign_keys", true)
        .expect("foreign keys must be enabled");
    connection
}

fn time(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

fn session_record(id: &str, parent_id: Option<&str>, created: u64) -> SessionRecord {
    SessionRecord {
        id: id.into(),
        parent_id: parent_id.map(String::from),
        status: SessionStatus::Running,
        failure_reason: None,
        delegated_to: None,
        total_event_bytes: created,
        created_at: time(created),
        updated_at: time(created),
    }
}

fn create_session(connection: &Connection, id: &str) {
    session::create(connection, &session_record(id, None, 1)).expect("session must be created");
}

#[test]
fn session_crud_preserves_values_and_lists_children_in_creation_order() {
    // Given: tempfile 上の移行済みデータベースと親セッション
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    let parent = session_record("parent", None, 1);

    // When: 親を作成して取得する
    session::create(&connection, &parent).expect("parent must be created");
    // Then: 全フィールドが同じ値で復元される
    assert_eq!(
        session::get(&connection, "parent").unwrap(),
        Some(parent.clone())
    );

    // When: 状態、委譲先、更新日時を変更する
    let updated = SessionRecord {
        status: SessionStatus::Delegated,
        delegated_to: Some("agent-a".into()),
        updated_at: time(2),
        ..parent.clone()
    };
    session::update(&connection, &updated).expect("parent must update");
    // Then: 更新後の全値が返る
    assert_eq!(session::get(&connection, "parent").unwrap(), Some(updated));

    // When: 作成日時の異なる子を逆順で登録する
    let children = [
        session_record("child-1", Some("parent"), 10),
        session_record("child-2", Some("parent"), 20),
        session_record("child-3", Some("parent"), 30),
    ];
    for child in children.iter().rev() {
        session::create(&connection, child).expect("child must be created");
    }
    // Then: 親参照が成立し、作成日時順で返る
    assert_eq!(
        session::list_by_parent(&connection, "parent").unwrap(),
        children
    );

    // When: 子を削除する
    let removed = session::delete(&connection, "child-2").expect("child must delete");
    // Then: 削除結果と非存在が返る
    assert!(removed);
    assert_eq!(session::get(&connection, "child-2").unwrap(), None);
}

#[test]
fn task_crud_supports_nullable_session_and_lists_by_session() {
    // Given: セッションと未所属タスク
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    create_session(&connection, "session-1");
    let detached = TaskRecord {
        id: "task-detached".into(),
        session_id: None,
        status: TaskStatus::Running,
        created_at: time(1),
        updated_at: time(1),
    };

    // When: 未所属タスクを作成して取得する
    task::create(&connection, &detached).expect("task must be created");
    // Then: NULL の所属を含め全値が復元される
    assert_eq!(
        task::get(&connection, "task-detached").unwrap(),
        Some(detached.clone())
    );

    // When: セッション所属かつ完了状態へ更新する
    let updated = TaskRecord {
        session_id: Some("session-1".into()),
        status: TaskStatus::Completed,
        updated_at: time(2),
        ..detached
    };
    task::update(&connection, &updated).expect("task must update");
    // Then: 更新後の全値が返る
    assert_eq!(
        task::get(&connection, "task-detached").unwrap(),
        Some(updated)
    );

    // When: 作成日時の異なる所属タスクを逆順で登録する
    let tasks = [10_u64, 20, 30].map(|created| TaskRecord {
        id: format!("task-{created}"),
        session_id: Some("session-1".into()),
        status: TaskStatus::Running,
        created_at: time(created),
        updated_at: time(created),
    });
    for record in tasks.iter().rev() {
        task::create(&connection, record).expect("task must be created");
    }
    // Then: 作成日時順で返る
    let mut expected = vec![task::get(&connection, "task-detached").unwrap().unwrap()];
    expected.extend(tasks);
    assert_eq!(
        task::list_by_session(&connection, "session-1").unwrap(),
        expected
    );

    // When: タスクを削除する
    assert!(task::delete(&connection, "task-20").unwrap());
    // Then: タスクは存在しない
    assert_eq!(task::get(&connection, "task-20").unwrap(), None);
}

#[test]
fn message_crud_enforces_foreign_key_and_lists_by_session() {
    // Given: セッションとメッセージ
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    create_session(&connection, "session-1");
    let original = MessageRecord {
        id: "message-1".into(),
        session_id: "session-1".into(),
        role: MessageRole::User,
        content: "question".into(),
        reasoning: None,
        created_at: time(1),
        updated_at: time(1),
    };

    // When: メッセージを作成して取得する
    message::create(&connection, &original).expect("message must be created");
    // Then: 全フィールドが同じ値で復元される
    assert_eq!(
        message::get(&connection, "message-1").unwrap(),
        Some(original.clone())
    );

    // When: 応答内容と更新日時を変更する
    let updated = MessageRecord {
        role: MessageRole::Assistant,
        content: "answer".into(),
        reasoning: Some("because".into()),
        updated_at: time(2),
        ..original
    };
    message::update(&connection, &updated).expect("message must update");
    // Then: 更新後の全値が返る
    assert_eq!(
        message::get(&connection, "message-1").unwrap(),
        Some(updated)
    );

    // When: 作成日時の異なるメッセージを逆順で登録する
    let messages = [10_u64, 20, 30].map(|created| MessageRecord {
        id: format!("message-{created}"),
        session_id: "session-1".into(),
        role: MessageRole::System,
        content: created.to_string(),
        reasoning: None,
        created_at: time(created),
        updated_at: time(created),
    });
    for record in messages.iter().rev() {
        message::create(&connection, record).expect("message must be created");
    }
    // Then: 作成日時順で返る
    let mut expected = vec![message::get(&connection, "message-1").unwrap().unwrap()];
    expected.extend(messages);
    assert_eq!(
        message::list_by_session(&connection, "session-1").unwrap(),
        expected
    );

    // When: 存在しないセッションを参照するメッセージを作成する
    let missing = MessageRecord {
        id: "missing".into(),
        session_id: "absent".into(),
        role: MessageRole::Tool,
        content: String::new(),
        reasoning: None,
        created_at: time(40),
        updated_at: time(40),
    };
    let error = message::create(&connection, &missing).expect_err("foreign key must fail");
    // Then: SQLite 外部キー違反として返る
    assert!(matches!(error, StorageError::Sqlite(_)));

    // When: メッセージを削除する
    assert!(message::delete(&connection, "message-20").unwrap());
    // Then: メッセージは存在しない
    assert_eq!(message::get(&connection, "message-20").unwrap(), None);
}

#[test]
fn agent_run_crud_preserves_values_and_lists_by_start_time() {
    // Given: セッションと実行記録
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    create_session(&connection, "session-1");
    let original = AgentRunRecord {
        id: "run-1".into(),
        session_id: "session-1".into(),
        provider: "provider-a".into(),
        model: "model-a".into(),
        status: AgentRunStatus::Running,
        started_at: time(1),
        finished_at: None,
    };

    // When: 実行記録を作成して取得する
    agent_run::create(&connection, &original).expect("run must be created");
    // Then: 全フィールドが同じ値で復元される
    assert_eq!(
        agent_run::get(&connection, "run-1").unwrap(),
        Some(original.clone())
    );

    // When: 完了状態と終了日時へ更新する
    let updated = AgentRunRecord {
        status: AgentRunStatus::Completed,
        finished_at: Some(time(2)),
        ..original
    };
    agent_run::update(&connection, &updated).expect("run must update");
    // Then: 更新後の全値が返る
    assert_eq!(agent_run::get(&connection, "run-1").unwrap(), Some(updated));

    // When: 開始日時の異なる実行記録を逆順で登録する
    let runs = [10_u64, 20, 30].map(|started| AgentRunRecord {
        id: format!("run-{started}"),
        session_id: "session-1".into(),
        provider: "provider-b".into(),
        model: "model-b".into(),
        status: AgentRunStatus::Failed,
        started_at: time(started),
        finished_at: Some(time(started + 1)),
    });
    for record in runs.iter().rev() {
        agent_run::create(&connection, record).expect("run must be created");
    }
    // Then: 開始日時順で返る
    let mut expected = vec![agent_run::get(&connection, "run-1").unwrap().unwrap()];
    expected.extend(runs);
    assert_eq!(
        agent_run::list_by_session(&connection, "session-1").unwrap(),
        expected
    );

    // When: 実行記録を削除する
    assert!(agent_run::delete(&connection, "run-20").unwrap());
    // Then: 実行記録は存在しない
    assert_eq!(agent_run::get(&connection, "run-20").unwrap(), None);
}

#[test]
fn update_missing_record_returns_sqlite_error() {
    // Given: 存在しないセッションの更新値
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let connection = open_connection(&temp_dir);
    let missing = session_record("missing", None, 1);

    // When: 存在しない ID を更新する
    let error = session::update(&connection, &missing).expect_err("missing update must fail");

    // Then: 更新対象なしを SQLite エラーとして返す
    assert_eq!(
        error,
        StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
    );
}
