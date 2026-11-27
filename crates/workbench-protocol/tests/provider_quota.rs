use workbench_protocol::{handshake::WireLimits, namespaces::diagnostics::*};
fn snapshot() -> ProviderQuotas {
    ProviderQuotas {
        profiles: vec![ProviderQuota {
            profile: "work".into(),
            provider: QuotaProvider::Codex,
            state: QuotaState::Ready,
            fetched_at_ms: Some(1),
            windows: vec![QuotaWindow {
                id: "primary".into(),
                label: "5h".into(),
                remaining_basis_points: Some(10000),
                resets_at_ms: Some(18000001),
                duration_seconds: Some(18000),
                usage_label: None,
            }],
            issue: None,
        }],
    }
}
#[test]
fn quota_limits_and_unknown_percent_are_validated() {
    let mut q = snapshot();
    q.validate().unwrap();
    for p in [None, Some(0), Some(10000)] {
        q.profiles[0].windows[0].remaining_basis_points = p;
        q.validate().unwrap();
    }
    q.profiles[0].windows[0].remaining_basis_points = Some(10001);
    assert!(q.validate().is_err());
    let mut q = snapshot();
    q.profiles[0].windows[0].label = "x".repeat(MAX_QUOTA_LABEL_BYTES + 1);
    assert!(q.validate().is_err());
    let mut q = snapshot();
    q.profiles[0].windows = vec![q.profiles[0].windows[0].clone(); MAX_QUOTA_WINDOWS + 1];
    assert!(q.validate().is_err());
    let mut q = snapshot();
    q.profiles = vec![q.profiles[0].clone(); MAX_PROVIDER_QUOTAS + 1];
    assert!(q.validate().is_err());
    let mut q = snapshot();
    q.profiles.push(q.profiles[0].clone());
    assert!(q.validate().is_err());
    assert_eq!(
        MAX_PROVIDER_QUOTAS,
        workbench_protocol::namespaces::settings::MAX_PROVIDERS
    );
}
#[test]
fn states_and_error_shape_cannot_smuggle_provider_text() {
    let mut q = snapshot();
    q.profiles[0].state = QuotaState::Stale;
    assert!(q.validate().is_err());
    q.profiles[0].issue = Some(QuotaIssue {
        code: QuotaIssueCode::Http,
        http_status: Some(503),
    });
    q.validate().unwrap();
    q.profiles[0].issue.as_mut().unwrap().http_status = Some(600);
    assert!(q.validate().is_err());
    assert!(
        serde_json::from_value::<QuotaIssue>(
            serde_json::json!({"code":"protocol", "body":"secret raw response"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<QuotaIssue>(
            serde_json::json!({"code":"protocol", "message":"secret"})
        )
        .is_err()
    );
    DiagnosticsQuery::GetProviderQuotas(GetProviderQuotas {})
        .validate(&WireLimits::default())
        .unwrap();
    DiagnosticsEvent::ProviderQuotasChanged {}
        .validate()
        .unwrap();
    ProviderQuotas::default().validate().unwrap();
}
