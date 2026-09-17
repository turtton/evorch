use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::prelude::*;

use super::*;

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Records {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[test]
fn cache_completion_records_expected_and_actual_once_without_a_bus() {
    // Given: a known stable prefix and a subscriber, without an EventBus.
    let records = Records::default();
    let subscriber = tracing_subscriber::registry().with(records.clone());
    let request: crate::message::ChatRequest = serde_json::from_value(serde_json::json!({
        "model": "test", "messages": [
            {"role": "system", "content": [{"type": "text", "text": "a".repeat(400)}]},
            {"role": "user", "content": [{"type": "text", "text": "new suffix"}]}
        ]
    }))
    .unwrap();
    // When: one attempt completes twice (terminal operations are idempotent).
    tracing::subscriber::with_default(subscriber, || {
        let mut observer = AttemptObserver::new(None, "test", None, "test", "test", false, None)
            .with_cache_expectation(&request);
        observer.emit_completed(
            &Usage {
                cache_read_tokens: 50,
                ..Usage::default()
            },
            FinishReason::Stop,
        );
        observer.emit_completed(&Usage::default(), FinishReason::Stop);
    });
    // Then: a single correlated record contains the estimate and actual count.
    let entries = records.0.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["expected_cacheable_tokens"], "100");
    assert_eq!(entries[0]["cache_read_tokens"], "50");
    assert_eq!(entries[0]["cache_hit_ratio"], "0.5");
    assert_eq!(entries[0]["cache_estimation"], "\"utf8_bytes_div_4\"");
    assert!(entries[0].contains_key("request_id"));
}
