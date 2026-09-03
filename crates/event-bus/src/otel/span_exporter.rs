// allow: SIZE_OK - in-module smoke テストの配置がタスク要件であり、emitter
// と provider 構築は同一 feature 生命周期の 1 単位。生産コード単体では
// 約200純 LOC (exporter.rs の先例に準拠)。
//! otel-exporter feature で有効化される span exporter 層。
//!
//! 写像層 ([`super::span`]) の出力 [`SpanAction`] 列を OpenTelemetry SDK の
//! tracer へ記録する [`OtelSpanEmitter`] と、OTLP HTTP / InMemory の tracer
//! provider 構築関数を提供する。写像層の admission (sampling / budget /
//! tombstone) 済みの action 列を消費するため、SDK 側 sampler は
//! [`Sampler::AlwaysOn`] 固定とする。
//!
//! # 親子付け
//!
//! mapper は論理 key ([`SpanKey`]) による親参照を返す。emitter は open 中の
//! span の [`SpanContext`] を保持し、`Context::with_remote_span_context`
//! 経由で親へ接続する (trace_id 継承と parent_span_id 連鎖は SDK が解決
//! する)。親 key が未知の場合は panic せず root として開始し、
//! `tracing::warn!` を出す (防御層。通常は mapper の `UnknownParent` drop
//! が止める)。
//!
//! # 非ゴール
//!
//! runtime への subscribe 配線は行わない (ADR 0014 の後続 slice)。

use std::collections::HashMap;
use std::time::SystemTime;

use opentelemetry::trace::{
    Span as _, SpanBuilder, SpanContext, SpanKind as OtelSpanKind, Status, TraceContextExt,
    Tracer as _, TracerProvider as _,
};
use opentelemetry::{Array, Context, KeyValue, StringValue, Value};
use opentelemetry_otlp::{ExporterBuildError, Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::{InMemorySpanExporter, Sampler, SdkTracer, SdkTracerProvider, Span};

use super::{SpanAction, SpanAttribute, SpanAttributeValue, SpanKey, SpanKind, SpanStatus};

/// span action を OTel tracer へ記録する emitter。
///
/// [`OtelSpanEmitter::apply`] は panic しない。未知の親 key による Start は
/// root span として開始し、未開始 key の End と既に open な key の再開始は
/// `tracing::warn!` で記録して続行する (防御層。通常は mapper の
/// `UnknownParent` / `UnknownSpanEnd` / `DuplicateSpan` drop が止める)。
pub struct OtelSpanEmitter {
    tracer: SdkTracer,
    open: HashMap<SpanKey, (Span, SpanContext)>,
}

impl OtelSpanEmitter {
    /// [`SdkTracerProvider`] から tracer を取得して emitter を構築する。
    pub fn new(provider: &SdkTracerProvider) -> Self {
        Self {
            tracer: provider.tracer("evorch.event-bus"),
            open: HashMap::new(),
        }
    }

    /// span action を 1 件記録する。
    pub fn apply(&mut self, action: SpanAction) {
        match action {
            SpanAction::Start {
                key,
                parent,
                name,
                kind,
                start_time,
                attributes,
            } => self.start(key, parent, name, kind, start_time, attributes),
            SpanAction::End {
                key,
                end_time,
                status,
                final_attributes,
            } => self.end(key, end_time, status, final_attributes),
        }
    }

    fn start(
        &mut self,
        key: SpanKey,
        parent: Option<SpanKey>,
        name: String,
        kind: SpanKind,
        start_time: SystemTime,
        attributes: Vec<SpanAttribute>,
    ) {
        if self.open.contains_key(&key) {
            tracing::warn!(
                span_key = ?key,
                "span start for an already-open key; replacing the open span"
            );
        }
        let parent_context = match parent {
            Some(parent_key) => match self.open.get(&parent_key) {
                Some((_, span_context)) => {
                    Some(Context::new().with_remote_span_context(span_context.clone()))
                }
                None => {
                    tracing::warn!(
                        span_key = ?key,
                        parent_key = ?parent_key,
                        "unknown parent span; starting as a root span"
                    );
                    None
                }
            },
            None => None,
        };
        let builder = SpanBuilder::from_name(name)
            .with_kind(map_span_kind(kind))
            .with_start_time(start_time)
            .with_attributes(attributes.iter().map(map_attribute));
        let span = match parent_context {
            Some(context) => self.tracer.build_with_context(builder, &context),
            None => self.tracer.build_with_context(builder, &Context::new()),
        };
        let span_context = span.span_context().clone();
        self.open.insert(key, (span, span_context));
    }

    fn end(
        &mut self,
        key: SpanKey,
        end_time: SystemTime,
        status: SpanStatus,
        final_attributes: Vec<SpanAttribute>,
    ) {
        let Some((mut span, _)) = self.open.remove(&key) else {
            tracing::warn!(
                span_key = ?key,
                "span end without a matching open span; ignoring"
            );
            return;
        };
        span.set_attributes(final_attributes.iter().map(map_attribute));
        span.set_status(map_status(status));
        span.end_with_timestamp(end_time);
    }
}

fn map_span_kind(kind: SpanKind) -> OtelSpanKind {
    match kind {
        SpanKind::Client => OtelSpanKind::Client,
        SpanKind::Internal => OtelSpanKind::Internal,
    }
}

fn map_status(status: SpanStatus) -> Status {
    match status {
        SpanStatus::Unset => Status::Unset,
        // 自由文字列の error description は非含有方針のため空とする
        // (失敗分類は error.type 属性が担う)。
        SpanStatus::Error => Status::error(""),
    }
}

fn map_attribute(attribute: &SpanAttribute) -> KeyValue {
    let value = match &attribute.value {
        SpanAttributeValue::Str(value) => Value::String(value.clone().into()),
        SpanAttributeValue::Strings(values) => Value::Array(Array::String(
            values.iter().cloned().map(StringValue::from).collect(),
        )),
        SpanAttributeValue::I64(value) => Value::I64(*value),
        SpanAttributeValue::F64(value) => Value::F64(value.get()),
        SpanAttributeValue::Bool(value) => Value::Bool(*value),
    };
    KeyValue::new(attribute.key.clone(), value)
}

/// OTLP traces signal path。
///
/// opentelemetry-otlp 0.32 の `with_endpoint` は URL を as-is で扱い
/// signal path の自動付加は行わない (env var 経由の場合のみ付加) ため、
/// base URL 契約を本 crate 側で正規化する。metrics の
/// `with_metrics_signal_path` と同一ポリシーの span 版。
const TRACES_SIGNAL_PATH: &str = "/v1/traces";

/// endpoint が traces signal path を持たない場合に付加する。
fn with_traces_signal_path(endpoint: &str) -> String {
    if endpoint.ends_with(TRACES_SIGNAL_PATH) {
        endpoint.to_owned()
    } else {
        // 末尾の連続 `/` を除去してから path を繋ぐ (二重スラッシュ防止)。
        format!("{}{TRACES_SIGNAL_PATH}", endpoint.trim_end_matches('/'))
    }
}

/// OTLP HTTP (protobuf) exporter を構築する。
///
/// `endpoint` への signal path 付加は [`build_otlp_tracer_provider`] と共通
/// ([`TRACES_SIGNAL_PATH`])。
///
/// # Errors
/// exporter の構築に失敗した場合 [`ExporterBuildError`] を返す。
pub fn build_otlp_span_exporter(endpoint: &str) -> Result<SpanExporter, ExporterBuildError> {
    SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(with_traces_signal_path(endpoint))
        .build()
}

/// OTLP HTTP (protobuf) exporter + [`SimpleSpanProcessor`](opentelemetry_sdk::trace::SimpleSpanProcessor)
/// で tracer provider を構築する。
///
/// `endpoint` は OTLP HTTP の base URL (例: `http://127.0.0.1:4318`) で、
/// traces signal path `/v1/traces` が無い場合は本関数が付加する
/// ([`TRACES_SIGNAL_PATH`])。mapper 側で admission (sampling / budget /
/// tombstone) が完了しているため、SDK sampler は [`Sampler::AlwaysOn`] 固定
/// とする。exporter は blocking client で同期送信される。
///
/// # Errors
/// exporter の構築に失敗した場合 [`ExporterBuildError`] を返す。0.32 では
/// `OTelSdkResult` が非ジェネリクスのため、metrics と同様に exporter 構築の
/// error 型をそのまま返す。
pub fn build_otlp_tracer_provider(endpoint: &str) -> Result<SdkTracerProvider, ExporterBuildError> {
    let exporter = build_otlp_span_exporter(endpoint)?;
    Ok(SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter)
        .build())
}

/// InMemory exporter 付きの tracer provider を構築する。
///
/// debug / テスト用途。span は [`OtelSpanEmitter`] の End 時に SimpleSpanProcessor
/// 経由で同期 export される。exporter がプロセス内メモリに滞留するため
/// production 向けではない (本番は [`build_otlp_tracer_provider`] を使う)。
pub fn build_in_memory_tracer_provider() -> (SdkTracerProvider, InMemorySpanExporter) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_simple_exporter(exporter.clone())
        .build();
    (provider, exporter)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use opentelemetry::Value;
    use opentelemetry::trace::{SpanId, SpanKind as OtelSpanKind, Status};
    use opentelemetry_sdk::trace::SpanData;

    use super::*;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn run_key() -> SpanKey {
        SpanKey::Run {
            run_id: "run-1".to_owned(),
        }
    }

    fn agent_key() -> SpanKey {
        SpanKey::Agent {
            run_id: "run-1".to_owned(),
        }
    }

    fn request_key() -> SpanKey {
        SpanKey::Request {
            request_id: "req-1".to_owned(),
        }
    }

    fn find_span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
        spans
            .iter()
            .find(|span| span.name.as_ref() == name)
            .unwrap_or_else(|| panic!("span {name} not found: spans={spans:?}"))
    }

    fn attribute_value<'a>(span: &'a SpanData, key: &str) -> &'a Value {
        &span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .unwrap_or_else(|| panic!("attribute {key} missing: {:?}", span.attributes))
            .value
    }

    fn attribute(span: &SpanData, key: &str) -> String {
        attribute_value(span, key).as_str().to_string()
    }

    // Given: in-memory tracer provider の emitter。
    // When: run → agent → request の 3 span を Start/End し force_flush する。
    // Then: 3 span が同一定 trace_id で parent_span_id 連鎖し、名前 / kind /
    //       指定時刻 / 属性 / status が写像どおり記録される。
    #[test]
    fn in_memory_smoke_records_three_span_tree() {
        let (provider, exporter) = build_in_memory_tracer_provider();
        let mut emitter = OtelSpanEmitter::new(&provider);

        emitter.apply(SpanAction::Start {
            key: run_key(),
            parent: None,
            name: "evorch.run core".to_owned(),
            kind: SpanKind::Internal,
            start_time: at(1),
            attributes: vec![SpanAttribute::new("evorch.agent.name", "core")],
        });
        emitter.apply(SpanAction::Start {
            key: agent_key(),
            parent: Some(run_key()),
            name: "invoke_agent core".to_owned(),
            kind: SpanKind::Client,
            start_time: at(2),
            attributes: vec![SpanAttribute::new("gen_ai.agent.name", "core")],
        });
        emitter.apply(SpanAction::Start {
            key: request_key(),
            parent: Some(agent_key()),
            name: "chat kimi-k3".to_owned(),
            kind: SpanKind::Client,
            start_time: at(3),
            attributes: vec![
                SpanAttribute::new("gen_ai.provider.name", "anthropic"),
                SpanAttribute::new("gen_ai.request.model", "kimi-k3"),
            ],
        });
        emitter.apply(SpanAction::End {
            key: request_key(),
            end_time: at(4),
            status: SpanStatus::Error,
            final_attributes: vec![
                SpanAttribute::new("gen_ai.provider.name", "anthropic"),
                SpanAttribute::new("gen_ai.request.model", "kimi-k3"),
                SpanAttribute::new("error.type", "timeout"),
                SpanAttribute::new("gen_ai.usage.input_tokens", 10i64),
            ],
        });
        emitter.apply(SpanAction::End {
            key: agent_key(),
            end_time: at(5),
            status: SpanStatus::Unset,
            final_attributes: vec![SpanAttribute::new("gen_ai.agent.name", "core")],
        });
        emitter.apply(SpanAction::End {
            key: run_key(),
            end_time: at(6),
            status: SpanStatus::Unset,
            final_attributes: vec![SpanAttribute::new("evorch.agent.name", "core")],
        });
        provider.force_flush().expect("force_flush succeeds");

        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 3, "spans={spans:?}");

        let run = find_span(&spans, "evorch.run core");
        let agent = find_span(&spans, "invoke_agent core");
        let request = find_span(&spans, "chat kimi-k3");

        assert_eq!(run.span_kind, OtelSpanKind::Internal);
        assert_eq!(agent.span_kind, OtelSpanKind::Client);
        assert_eq!(request.span_kind, OtelSpanKind::Client);

        assert_eq!(run.parent_span_id, SpanId::INVALID);
        assert_eq!(agent.parent_span_id, run.span_context.span_id());
        assert_eq!(request.parent_span_id, agent.span_context.span_id());
        assert_eq!(run.span_context.trace_id(), agent.span_context.trace_id());
        assert_eq!(
            agent.span_context.trace_id(),
            request.span_context.trace_id()
        );

        assert_eq!(run.start_time, at(1));
        assert_eq!(agent.start_time, at(2));
        assert_eq!(request.end_time, at(4));
        assert_eq!(run.end_time, at(6));

        assert_eq!(attribute(run, "evorch.agent.name"), "core");
        assert_eq!(attribute(agent, "gen_ai.agent.name"), "core");
        assert_eq!(attribute(request, "gen_ai.request.model"), "kimi-k3");
        assert_eq!(attribute(request, "error.type"), "timeout");
        match attribute_value(request, "gen_ai.usage.input_tokens") {
            Value::I64(value) => assert_eq!(*value, 10),
            other => panic!("i64 attribute expected: {other:?}"),
        }
        assert_eq!(run.status, Status::Unset);
        assert_eq!(agent.status, Status::Unset);
        assert_eq!(request.status, Status::error(""));
    }

    // Given: in-memory tracer provider の emitter。
    // When: string 配列属性 (gen_ai.response.finish_reasons) を伴う request
    //       span を Start/End し force_flush する。
    // Then: 属性は OTel Value::Array (Array::String) として記録され、要素列
    //       が保持される。
    #[test]
    fn in_memory_smoke_maps_string_array_attribute_to_otl_value_array() {
        let (provider, exporter) = build_in_memory_tracer_provider();
        let mut emitter = OtelSpanEmitter::new(&provider);

        emitter.apply(SpanAction::Start {
            key: request_key(),
            parent: None,
            name: "chat kimi-k3".to_owned(),
            kind: SpanKind::Client,
            start_time: at(1),
            attributes: vec![],
        });
        emitter.apply(SpanAction::End {
            key: request_key(),
            end_time: at(2),
            status: SpanStatus::Unset,
            final_attributes: vec![SpanAttribute::new(
                "gen_ai.response.finish_reasons",
                vec!["stop".to_owned()],
            )],
        });
        provider.force_flush().expect("force_flush succeeds");

        let spans = exporter.get_finished_spans().expect("finished spans");
        let request = find_span(&spans, "chat kimi-k3");
        match attribute_value(request, "gen_ai.response.finish_reasons") {
            Value::Array(Array::String(values)) => {
                assert_eq!(
                    values,
                    &["stop".to_owned().into()],
                    "finish_reasons must be a 1-element string array"
                );
            }
            other => panic!("string array attribute expected: {other:?}"),
        }
    }

    // Given: 親 agent key が open state に存在しない Start。
    // When: 開始して終了する。
    // Then: panic せず root span として 1 件記録される。
    #[test]
    fn start_with_unknown_parent_starts_root_span_without_panic() {
        let (provider, exporter) = build_in_memory_tracer_provider();
        let mut emitter = OtelSpanEmitter::new(&provider);

        emitter.apply(SpanAction::Start {
            key: request_key(),
            parent: Some(SpanKey::Agent {
                run_id: "missing".to_owned(),
            }),
            name: "chat kimi-k3".to_owned(),
            kind: SpanKind::Client,
            start_time: at(1),
            attributes: vec![],
        });
        emitter.apply(SpanAction::End {
            key: request_key(),
            end_time: at(2),
            status: SpanStatus::Unset,
            final_attributes: vec![],
        });
        provider.force_flush().expect("force_flush succeeds");

        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 1, "spans={spans:?}");
        assert_eq!(spans[0].parent_span_id, SpanId::INVALID);
    }

    // Given: Start されていない key の End。
    // When: End を apply する。
    // Then: panic せず何も export されない。
    #[test]
    fn end_without_start_is_noop() {
        let (provider, exporter) = build_in_memory_tracer_provider();
        let mut emitter = OtelSpanEmitter::new(&provider);

        emitter.apply(SpanAction::End {
            key: run_key(),
            end_time: at(1),
            status: SpanStatus::Unset,
            final_attributes: vec![],
        });
        provider.force_flush().expect("force_flush succeeds");

        assert!(
            exporter
                .get_finished_spans()
                .expect("finished spans")
                .is_empty()
        );
    }

    // Given: 同一 key を 2 回 End する。
    // When: Start → End → End を apply する。
    // Then: panic せず export は 1 件のまま。
    #[test]
    fn double_end_exports_single_span() {
        let (provider, exporter) = build_in_memory_tracer_provider();
        let mut emitter = OtelSpanEmitter::new(&provider);

        emitter.apply(SpanAction::Start {
            key: run_key(),
            parent: None,
            name: "evorch.run core".to_owned(),
            kind: SpanKind::Internal,
            start_time: at(1),
            attributes: vec![],
        });
        emitter.apply(SpanAction::End {
            key: run_key(),
            end_time: at(2),
            status: SpanStatus::Unset,
            final_attributes: vec![],
        });
        emitter.apply(SpanAction::End {
            key: run_key(),
            end_time: at(3),
            status: SpanStatus::Unset,
            final_attributes: vec![],
        });
        provider.force_flush().expect("force_flush succeeds");

        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 1, "spans={spans:?}");
    }

    // Given: signal path を持たない / 末尾スラッシュ付き / 既に path 付きの
    //        3 endpoint。
    // When: with_traces_signal_path で正規化する。
    // Then: /v1/traces が 1 回だけ付加され、既に付与済みなら as-is。
    #[test]
    fn normalizes_endpoint_with_traces_signal_path() {
        assert_eq!(
            with_traces_signal_path("http://127.0.0.1:4318"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            with_traces_signal_path("http://127.0.0.1:4318/"),
            "http://127.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            with_traces_signal_path("http://127.0.0.1:4318/v1/traces"),
            "http://127.0.0.1:4318/v1/traces"
        );
    }

    // Given: OTLP HTTP base URL。
    // When: build_otlp_tracer_provider で構築する。
    // Then: HttpBinary exporter 付き provider が構築される (送信は発生しない)。
    #[test]
    fn otlp_tracer_provider_builds_with_http_binary_exporter() {
        let provider =
            build_otlp_tracer_provider("http://127.0.0.1:4318").expect("provider builds");

        provider.force_flush().expect("force_flush succeeds");
        provider.shutdown().expect("shutdown succeeds");
    }
}
