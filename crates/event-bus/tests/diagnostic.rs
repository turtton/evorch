use event_bus::otel::span::{
    SpanAction, SpanAttribute, SpanMapper, SpanStatus, validate_span_attributes,
};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventKind};

fn diagnostic(severity: DiagnosticSeverity) -> Event {
    Event::new(DiagnosticEvent {
        source: "process_owner".into(),
        severity,
        code: "claim_conflict".into(),
        detail: "Another owner holds the claim".into(),
        run_id: Some("run-1".into()),
        thread_id: Some("thread-1".into()),
    })
}

#[test]
fn diagnostic_round_trips_with_optional_correlations() {
    // Given: all severities, with and without correlation identifiers.
    for severity in [
        DiagnosticSeverity::Info,
        DiagnosticSeverity::Warning,
        DiagnosticSeverity::Error,
    ] {
        for correlated in [true, false] {
            let mut event = diagnostic(severity);
            if let EventKind::Diagnostic(value) = &mut event.kind
                && !correlated
            {
                value.run_id = None;
                value.thread_id = None;
            }
            // When: crossing the JSON boundary.
            let json = serde_json::to_value(&event).expect("serialize");
            let restored: Event = serde_json::from_value(json.clone()).expect("deserialize");
            // Then: the tagged variant and full envelope survive.
            assert_eq!(json["kind"]["kind"], "Diagnostic");
            assert_eq!(restored, event);
        }
    }
}

#[test]
fn diagnostic_maps_to_a_closed_span_without_exporting_detail() {
    // Given: a standalone error diagnostic and a fresh mapper.
    let event = diagnostic(DiagnosticSeverity::Error);
    let mut mapper = SpanMapper::new();
    // When: ingesting the event twice.
    let actions = mapper.ingest(&event);
    let repeated = mapper.ingest(&event);
    // Then: each occurrence is a distinct, immediately closed diagnostic span.
    let [
        SpanAction::Start {
            key,
            name,
            attributes,
            ..
        },
        SpanAction::End {
            key: ended,
            status,
            final_attributes,
            ..
        },
    ] = actions.as_slice()
    else {
        panic!("expected a complete span: {actions:?}");
    };
    assert_eq!(name, "evorch.diagnostic");
    assert_eq!(key, ended);
    assert_eq!(*status, SpanStatus::Error);
    assert_eq!(attributes, final_attributes);
    assert!(attributes.contains(&SpanAttribute::new(
        "evorch.diagnostic.source",
        "process_owner"
    )));
    assert!(attributes.contains(&SpanAttribute::new("evorch.diagnostic.severity", "error")));
    assert!(attributes.contains(&SpanAttribute::new(
        "evorch.diagnostic.code",
        "claim_conflict"
    )));
    assert!(attributes.contains(&SpanAttribute::new("evorch.thread.id", "thread-1")));
    assert!(!format!("{attributes:?}").contains("Another owner"));
    validate_span_attributes(attributes).expect("valid diagnostic attributes");
    assert!(matches!(&repeated[0], SpanAction::Start { key: next, .. } if next != key));
    assert!(mapper.drain_drops().is_empty());
}

#[test]
fn diagnostic_recovers_after_admission_window_resets() {
    // Given: one diagnostic occurrence is allowed per admission window.
    let mut mapper = SpanMapper::with_budget(event_bus::otel::span::SpanBudget {
        max_admitted_spans_per_window: 1,
        ..Default::default()
    });
    let mut event = diagnostic(DiagnosticSeverity::Info);
    assert_eq!(mapper.ingest(&event).len(), 2);
    assert!(mapper.ingest(&event).is_empty());
    event.meta.wall_clock += std::time::Duration::from_secs(61);
    // When: a new occurrence arrives after the window resets.
    let actions = mapper.ingest(&event);
    // Then: a previous rejected occurrence cannot suppress its close action.
    assert!(matches!(
        actions.as_slice(),
        [
            SpanAction::Start { .. },
            SpanAction::End {
                status: SpanStatus::Unset,
                ..
            }
        ]
    ));
}
