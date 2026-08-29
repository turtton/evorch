//! ADR 0008 の credential 非永続化 — イベントペイロードの JSON key を検証します。

use std::collections::BTreeSet;

use event_bus::{
    EventKind, FaultEvent, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent, UsageEvent,
};
use serde_json::Value;

fn event_kinds() -> [EventKind; 14] {
    [
        LifecycleEvent::Started {
            session_id: "s".into(),
        }
        .into(),
        LifecycleEvent::Delegated {
            session_id: "s".into(),
            target: "worker".into(),
        }
        .into(),
        LifecycleEvent::BackgroundTaskStarted {
            task_id: "t".into(),
        }
        .into(),
        LifecycleEvent::BackgroundTaskCompleted {
            task_id: "t".into(),
        }
        .into(),
        LifecycleEvent::Completed {
            session_id: "s".into(),
        }
        .into(),
        LifecycleEvent::Failed {
            session_id: "s".into(),
            reason: "failed".into(),
        }
        .into(),
        MessageEvent::MessageDelta {
            delta: "text".into(),
        }
        .into(),
        MessageEvent::ReasoningDelta {
            delta: "thought".into(),
        }
        .into(),
        ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "c".into(),
        }
        .into(),
        ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "c".into(),
            is_error: false,
        }
        .into(),
        UsageEvent::Usage {
            provider: "p".into(),
            model: "m".into(),
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
        }
        .into(),
        UsageEvent::CacheStats {
            provider: "p".into(),
            model: "m".into(),
            cache_hits: 5,
            cache_misses: 6,
        }
        .into(),
        ProviderEvent::ProviderFallback {
            from_provider: "p".into(),
            to_provider: "q".into(),
            reason: "retry".into(),
        }
        .into(),
        FaultEvent::SubscriberLagged {
            subscriber_id: 7,
            skipped: 8,
        }
        .into(),
    ]
}

fn collect_keys(value: &Value, keys: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, nested) in object {
                keys.insert(key.clone());
                collect_keys(nested, keys);
            }
        }
        Value::Array(values) => values.iter().for_each(|value| collect_keys(value, keys)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn every_event_kind_json_key_is_in_the_credential_free_allowlist() {
    // Given: EventKind 全 14 variant と credential を含まない固定 key allowlist
    let allowed = BTreeSet::from(
        [
            "kind",
            "payload",
            "session_id",
            "target",
            "task_id",
            "reason",
            "delta",
            "tool_name",
            "call_id",
            "is_error",
            "provider",
            "model",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "cache_hits",
            "cache_misses",
            "from_provider",
            "to_provider",
            "subscriber_id",
            "skipped",
            "schema_version",
            "monotonic",
            "secs",
            "nanos",
            "wall_clock",
        ]
        .map(String::from),
    );

    // When: 各 variant を JSON object 化して key を再帰収集する
    let mut actual = BTreeSet::new();
    for kind in event_kinds() {
        let json = serde_json::to_string(&kind).expect("event kind must serialize");
        let Value::Object(object) = serde_json::from_str(&json).expect("JSON must parse") else {
            panic!("event kind must serialize to an object");
        };
        collect_keys(&Value::Object(object), &mut actual);
    }

    // Then: 観測された key は allowlist の部分集合である
    assert!(
        actual.is_subset(&allowed),
        "unexpected JSON keys: {:?}",
        actual.difference(&allowed)
    );
}
