use std::time::{Duration, UNIX_EPOCH};

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventMeta,
    LifecycleEvent, MessageEvent,
};
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;

fn event(kind: impl Into<event_bus::EventKind>, seconds: u64) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(seconds),
            wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
        },
        kind: kind.into(),
    }
}

fn message(message_id: &str, sender_run_id: &str, recipient_run_id: &str) -> AgentMessage {
    AgentMessage {
        message_id: message_id.into(),
        sender_run_id: sender_run_id.into(),
        recipient_run_id: recipient_run_id.into(),
        kind: AgentMessageKind::Send,
        content: format!("content-{message_id}"),
        reply_to: None,
    }
}

#[test]
fn agent_messages_by_session_restores_order_and_correlation() {
    // Given: 他種イベントと別セッションを挟む三件の AgentMessage 配送
    let temp_dir = TempDir::new().expect("temporary directory must be created");
    let config = StorageConfig {
        db_path: temp_dir.path().join("agent-messages.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage must open");
    let handle = storage.handle();
    let send = message("msg-1", "run-1", "run-2");
    let steering = AgentMessage {
        kind: AgentMessageKind::Steering,
        ..message("msg-2", "run-1", "run-2")
    };
    let reply = message("msg-3", "run-2", "run-1");
    let reply = AgentMessage {
        kind: AgentMessageKind::Reply,
        reply_to: Some("msg-1".into()),
        ..reply
    };
    handle
        .append_event(
            Some("s1"),
            &event(
                AgentMessageEvent::Delivered {
                    message: send.clone(),
                    disposition: DeliveryDisposition::Aside,
                },
                1,
            ),
        )
        .expect("send must append");
    handle
        .append_event(
            Some("s1"),
            &event(
                MessageEvent::MessageDelta {
                    delta: "interleaved".into(),
                },
                2,
            ),
        )
        .expect("message must append");
    handle
        .append_event(
            Some("s1"),
            &event(
                AgentMessageEvent::Delivered {
                    message: steering.clone(),
                    disposition: DeliveryDisposition::Steering,
                },
                3,
            ),
        )
        .expect("steering must append");
    handle
        .append_event(
            Some("s2"),
            &event(
                AgentMessageEvent::Delivered {
                    message: message("other", "run-3", "run-4"),
                    disposition: DeliveryDisposition::Wake,
                },
                4,
            ),
        )
        .expect("other-session message must append");
    handle
        .append_event(
            Some("s1"),
            &event(
                LifecycleEvent::Completed {
                    session_id: "s1".into(),
                },
                5,
            ),
        )
        .expect("lifecycle must append");
    handle
        .append_event(
            Some("s1"),
            &event(
                AgentMessageEvent::Delivered {
                    message: reply.clone(),
                    disposition: DeliveryDisposition::Wake,
                },
                6,
            ),
        )
        .expect("reply must append");
    storage.close();
    let database = Database::open(&config).expect("database must open");

    // When: セッション s1 の AgentMessage 配送を復元する
    let actual = database
        .agent_messages_by_session("s1")
        .expect("agent messages must restore");

    // Then: 挿入順で相関情報と配送区分を保ち、他種・別セッションを含まない
    assert_eq!(actual.len(), 3);
    assert_eq!(actual[0].message, send);
    assert_eq!(actual[0].disposition, DeliveryDisposition::Aside);
    assert_eq!(actual[1].message, steering);
    assert_eq!(actual[1].disposition, DeliveryDisposition::Steering);
    assert_eq!(actual[2].message, reply);
    assert_eq!(actual[2].disposition, DeliveryDisposition::Wake);
}
