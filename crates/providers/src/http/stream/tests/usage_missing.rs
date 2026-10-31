//! provider が終端で usage を報告しなかった場合、ポンプは使用量をゼロ報告せず、
//! cache 観測も出さずに完了させ、診断で未報告を 1 件だけ記録する。

use super::*;
use crate::message::ObservationContext;
use futures_util::FutureExt;

/// `[DONE]` で完了し、`usage_reported` を固定で返す偽インタプリタ。
struct Reporting(bool);

impl WireStreamInterpreter for Reporting {
    fn interpret(&mut self, _frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        let usage = if self.0 {
            Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            }
        } else {
            Usage::default()
        };
        Ok(FrameInterpretation {
            events: Vec::new(),
            completion: Some((usage, FinishReason::Stop)),
        })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }

    fn usage_reported(&self) -> bool {
        self.0
    }
}

#[derive(Default)]
struct Counts {
    usage: usize,
    cache_reuse: usize,
    completed: usize,
    completed_reports_usage: Option<bool>,
    diagnostics: Vec<String>,
}

fn run(reported: bool) -> (Counts, Vec<Result<StreamEvent, ProviderError>>) {
    let bus = Arc::new(EventBus::new(16));
    let mut rx = bus.subscribe();
    let request = serde_json::json!({"model": "m", "messages": [{"role": "user", "content": "x"}]});
    let observer = AttemptObserver::new(
        Some(bus.clone()),
        "test",
        None,
        "test-protocol",
        "model-a",
        true,
        Some(ObservationContext {
            run_id: "run-1".into(),
            ..ObservationContext::default()
        }),
    )
    .with_cache_observation(&request);
    let mut pump = SsePump::new(
        Reporting(reported),
        UsageEmitter::new(Some(bus), "test"),
        "model-a".to_string(),
        observer,
    );

    pump.push_chunk(b"data: {}\n\n");

    let mut counts = Counts::default();
    while let Some(event) = rx.recv().now_or_never() {
        match event.expect("test events must not lag").kind {
            EventKind::Usage(UsageEvent::Usage { .. }) => counts.usage += 1,
            EventKind::Provider(ProviderEvent::CacheReuseObserved { .. }) => {
                counts.cache_reuse += 1
            }
            EventKind::Provider(ProviderEvent::RequestCompleted { usage_reported, .. }) => {
                counts.completed += 1;
                counts.completed_reports_usage = Some(usage_reported);
            }
            EventKind::Diagnostic(diagnostic) => {
                assert_eq!(diagnostic.run_id.as_deref(), Some("run-1"));
                counts.diagnostics.push(diagnostic.code);
            }
            _ => {}
        }
    }
    (counts, std::iter::from_fn(|| pump.pop_pending()).collect())
}

// Given: usage を報告しない完了 / When: pump が完了を受理 / Then: UsageEvent と
// CacheReuseObserved は 0 件、RequestCompleted は 1 件、診断は ProviderUsageMissing が 1 件
#[test]
fn missing_usage_skips_usage_and_cache_observation_but_still_completes() {
    let (counts, events) = run(false);

    assert_eq!(counts.usage, 0);
    assert_eq!(counts.cache_reuse, 0);
    assert_eq!(counts.completed, 1);
    assert_eq!(counts.completed_reports_usage, Some(false));
    assert_eq!(counts.diagnostics, ["ProviderUsageMissing"]);
    let [Ok(StreamEvent::Completed { response })] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(response.usage, Usage::default());
    assert!(!response.usage_reported);
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

// Given: usage を報告する完了 (対照) / When: 同じ経路 / Then: UsageEvent と
// CacheReuseObserved が 1 件ずつ出て、未報告診断は出ない
#[test]
fn reported_usage_emits_usage_and_cache_observation_without_diagnostic() {
    let (counts, _) = run(true);

    assert_eq!(counts.usage, 1);
    assert_eq!(counts.cache_reuse, 1);
    assert_eq!(counts.completed, 1);
    assert_eq!(counts.completed_reports_usage, Some(true));
    assert!(counts.diagnostics.is_empty(), "{:?}", counts.diagnostics);
}
