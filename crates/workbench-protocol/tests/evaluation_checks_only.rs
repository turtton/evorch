//! The `checks_only` flag of `StartEvaluationReport`: wire spelling, readback and validation.
use serde_json::json;
use workbench_protocol::namespaces::evaluation::StartEvaluationReport;

fn start(extra: serde_json::Value) -> StartEvaluationReport {
    let mut value = json!({"operation_id": "op-1", "targets": ["run-1"]});
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn an_older_request_without_the_flag_is_a_plain_report_and_the_flag_is_written_only_when_set() {
    let plain = start(json!({}));
    assert!(!plain.checks_only);
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("checks_only")
            .is_none()
    );

    let checks = start(json!({"checks_only": true}));
    assert!(checks.checks_only);
    assert_eq!(serde_json::to_value(&checks).unwrap()["checks_only"], true);
    checks.validate().unwrap();
}

#[test]
fn checks_only_judges_nothing_so_it_cannot_rejudge() {
    let both = start(json!({"checks_only": true, "rejudge": true}));

    let error = both.validate().unwrap_err();

    assert_eq!(error.field(), "checks_only");
}
