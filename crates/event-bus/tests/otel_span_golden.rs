//! span mapper の golden テスト。
//!
//! fixture は実装出力から生成せず手書きで維持する (`otel_golden.rs` の先例に
//! 倣う)。`tests/otel_span_golden/*.json` を走査し、recording sink で
//! Start/End action を完成 span 記録へ正規化した上で、span 列 (key/start_ms
//! 順) と drop 列 (kind/key 順) を期待値と完全一致比較する。属性配列は
//! 生成順のまま比較する。`config.budget` の `max_span_lifetime` のみ
//! ミリ秒数で表現する (他の budget キーは回数上限)。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_bus::otel::{
    SpanAction, SpanAttribute, SpanAttributeValue, SpanBudget, SpanDrop, SpanDropKind, SpanKey,
    SpanKind, SpanMapper, SpanStatus,
};
use event_bus::{Event, EventKind, EventMeta, SCHEMA_VERSION};
use serde_json::{Value, json};

fn golden_cases() -> Vec<(String, Value)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/otel_span_golden");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .expect("golden fixture directory exists")
        .map(|entry| entry.expect("fixture entry readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .expect("fixture file name")
                .to_string_lossy()
                .into_owned();
            let content = std::fs::read_to_string(&path).expect("fixture readable");
            let value: Value = serde_json::from_str(&content).expect("valid fixture JSON");
            (name, value)
        })
        .collect()
}

fn key_string(key: &SpanKey) -> String {
    match key {
        SpanKey::Run { run_id } => format!("run:{run_id}"),
        SpanKey::Agent { run_id } => format!("agent:{run_id}"),
        SpanKey::Request { request_id } => format!("request:{request_id}"),
        SpanKey::Tool { call_id } => format!("tool:{call_id}"),
        SpanKey::Session { session_id } => format!("session:{session_id}"),
    }
}

fn millis(at: SystemTime) -> u64 {
    let elapsed = at
        .duration_since(UNIX_EPOCH)
        .expect("event time at or after epoch");
    u64::try_from(elapsed.as_millis()).expect("millis fit u64")
}

fn attribute_json(attribute: &SpanAttribute) -> Value {
    let value = match &attribute.value {
        SpanAttributeValue::Str(value) => json!(value),
        SpanAttributeValue::I64(value) => json!(value),
        SpanAttributeValue::F64(value) => json!(value.get()),
        SpanAttributeValue::Bool(value) => json!(value),
    };
    json!([attribute.key, value])
}

fn attributes_json(attributes: &[SpanAttribute]) -> Value {
    Value::Array(attributes.iter().map(attribute_json).collect())
}

fn build_mapper(config: &Value) -> SpanMapper {
    let ratio = config.get("sampling_ratio").and_then(Value::as_f64);
    let budget = config.get("budget");
    assert!(
        ratio.is_none() || budget.is_none(),
        "sampling_ratio and budget are mutually exclusive (public API is either-or)"
    );
    if let Some(ratio) = ratio {
        return SpanMapper::with_sampling_ratio(ratio);
    }
    let Some(spec) = budget.and_then(Value::as_object) else {
        return SpanMapper::new();
    };
    let mut limits = SpanBudget::default();
    for (key, value) in spec {
        let count = value
            .as_u64()
            .unwrap_or_else(|| panic!("budget key {key} must be a count"));
        let count = usize::try_from(count)
            .unwrap_or_else(|error| panic!("budget key {key} overflows usize: {error}"));
        match key.as_str() {
            "max_in_flight_spans_per_run" => limits.max_in_flight_spans_per_run = count,
            "max_in_flight_spans_global" => limits.max_in_flight_spans_global = count,
            "max_admitted_spans_per_window" => limits.max_admitted_spans_per_window = count,
            "max_span_lifetime" => limits.max_span_lifetime = Duration::from_millis(count as u64),
            "max_attributes_per_span" => limits.max_attributes_per_span = count,
            "max_attribute_bytes_per_span" => limits.max_attribute_bytes_per_span = count,
            "max_attribute_value_bytes" => limits.max_attribute_value_bytes = count,
            other => panic!("unknown budget key: {other}"),
        }
    }
    SpanMapper::with_budget(limits)
}

struct SpanRecord {
    key: String,
    parent: Option<String>,
    name: String,
    kind: &'static str,
    start_ms: u64,
    end_ms: Option<u64>,
    status: &'static str,
    attributes: Value,
}

fn record(sink: &mut Vec<SpanRecord>, action: SpanAction) {
    match action {
        SpanAction::Start {
            key,
            parent,
            name,
            kind,
            start_time,
            attributes,
        } => {
            sink.push(SpanRecord {
                key: key_string(&key),
                parent: parent.as_ref().map(key_string),
                name,
                kind: match kind {
                    SpanKind::Client => "client",
                    SpanKind::Internal => "internal",
                },
                start_ms: millis(start_time),
                end_ms: None,
                status: "unset",
                attributes: attributes_json(&attributes),
            });
        }
        SpanAction::End {
            key,
            end_time,
            status,
            final_attributes,
        } => {
            let key = key_string(&key);
            let record = sink
                .iter_mut()
                .rev()
                .find(|record| record.key == key && record.end_ms.is_none())
                .expect("End action follows its visible Start action");
            record.end_ms = Some(millis(end_time));
            record.status = match status {
                SpanStatus::Unset => "unset",
                SpanStatus::Error => "error",
            };
            record.attributes = attributes_json(&final_attributes);
        }
    }
}

fn record_json(record: &SpanRecord) -> Value {
    json!({
        "key": record.key,
        "parent": record.parent,
        "name": record.name,
        "kind": record.kind,
        "start_ms": record.start_ms,
        "end_ms": record.end_ms,
        "status": record.status,
        "attributes": record.attributes,
    })
}

fn drop_json(drop: &SpanDrop) -> Value {
    let kind = match drop.kind {
        SpanDropKind::MissingRunId => "missing_run_id",
        SpanDropKind::UnknownParent => "unknown_parent",
        SpanDropKind::UnknownSpanEnd => "unknown_span_end",
        SpanDropKind::DuplicateSpan => "duplicate_span",
        SpanDropKind::SampledOut => "sampled_out",
        SpanDropKind::BudgetInFlightPerRun => "budget_in_flight_per_run",
        SpanDropKind::BudgetInFlightGlobal => "budget_in_flight_global",
        SpanDropKind::BudgetWindow => "budget_window",
        SpanDropKind::BudgetAttributes => "budget_attributes",
        SpanDropKind::BudgetEvicted => "budget_evicted",
    };
    json!({"kind": kind, "key": key_string(&drop.key)})
}

// Given: tests/otel_span_golden/*.json の全 fixture。
// When: config から mapper を組み立て、events を at_ms (UNIX_EPOCH 起点ミリ秒)
//       で復元して順に ingest する。
// Then: 完成した span 記録列 (key/start_ms 順) と drop 記録列 (kind/key 順) が
//       期待値と完全一致する (属性の生成順を含む)。
#[test]
fn span_golden_fixtures_match_the_mapping_contract() {
    for (name, fixture) in golden_cases() {
        let mut mapper = build_mapper(fixture.get("config").unwrap_or(&Value::Null));
        let mut sink = Vec::new();
        for event in fixture["events"].as_array().expect("events array") {
            let at_ms = event["at_ms"].as_u64().expect("at_ms");
            let kind: EventKind = serde_json::from_value(event["kind"].clone())
                .unwrap_or_else(|error| panic!("{name}: event deserialization failed: {error}"));
            let at = Duration::from_millis(at_ms);
            let event = Event {
                meta: EventMeta {
                    schema_version: SCHEMA_VERSION,
                    monotonic: at,
                    wall_clock: UNIX_EPOCH + at,
                },
                kind,
            };
            for action in mapper.ingest(&event) {
                record(&mut sink, action);
            }
        }
        let mut spans: Vec<Value> = sink.iter().map(record_json).collect();
        spans.sort_by(|left, right| {
            (left["key"].as_str(), left["start_ms"].as_u64())
                .cmp(&(right["key"].as_str(), right["start_ms"].as_u64()))
        });
        assert_eq!(
            Value::Array(spans),
            fixture["expected_spans"],
            "span mismatch: {name}"
        );

        let mut drops: Vec<Value> = mapper.drain_drops().iter().map(drop_json).collect();
        drops.sort_by(|left, right| {
            (left["kind"].as_str(), left["key"].as_str())
                .cmp(&(right["kind"].as_str(), right["key"].as_str()))
        });
        let expected_drops = fixture["expected_drops"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            Value::Array(drops),
            Value::Array(expected_drops),
            "drop mismatch: {name}"
        );
    }
}
