use std::panic::{AssertUnwindSafe, catch_unwind};

use gui::app::WorkbenchState;
use gui::evidence::{AdapterPolicy, Decision, capture_or_skip, decide};
use gui::headless::{CapturedFrame, HeadlessWorkbench, OffscreenError};
use runtime::AgentSummary;
use workspace_ui::UiSettings;

#[derive(Clone)]
struct EmptySource;

impl gui::model::tasks::AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

fn workbench() -> HeadlessWorkbench<EmptySource> {
    let state = WorkbenchState::new(EmptySource, &UiSettings::default())
        .expect("default workbench state must build");
    HeadlessWorkbench::new(state, [640.0, 360.0])
}

#[test]
fn adapter_policy_from_value_defaults_to_skip_unless_explicitly_required() {
    // Given: unset, disabled, empty, and explicitly-required environment values
    let cases = [
        (None, AdapterPolicy::SkipIfMissing),
        (Some("0"), AdapterPolicy::SkipIfMissing),
        (Some(""), AdapterPolicy::SkipIfMissing),
        (Some("1"), AdapterPolicy::Require),
    ];

    // When: each value is parsed at the environment boundary
    // Then: only "1" requires an adapter
    for (value, expected) in cases {
        assert_eq!(AdapterPolicy::from_value(value), expected);
    }
}

#[test]
fn decide_skips_only_adapter_unavailability_when_policy_allows_it() {
    // Given: an adapter-unavailable capture result
    let result = Err(OffscreenError::AdapterUnavailable(
        "no adapter found".to_owned(),
    ));

    // When: the skip policy decides its outcome
    let decision = decide(AdapterPolicy::SkipIfMissing, &result);

    // Then: evidence is skipped with the renderer's reason
    assert!(matches!(decision, Decision::Skip(message) if message.contains("no adapter found")));
}

#[test]
fn decide_fails_adapter_unavailability_when_policy_requires_it() {
    // Given: an adapter-unavailable capture result
    let result = Err(OffscreenError::AdapterUnavailable(
        "no adapter found".to_owned(),
    ));

    // When: the required-adapter policy decides its outcome
    let decision = decide(AdapterPolicy::Require, &result);

    // Then: CI receives an actionable failure
    assert!(
        matches!(decision, Decision::Fail(message) if message.contains("EVORCH_REQUIRE_ADAPTER"))
    );
}

#[test]
fn decide_fails_render_and_encode_errors_under_every_policy() {
    // Given: non-adapter rendering failures and both adapter policies
    let results = [
        Err(OffscreenError::Render("render failed".to_owned())),
        Err(OffscreenError::Encode("encode failed".to_owned())),
    ];
    let policies = [AdapterPolicy::SkipIfMissing, AdapterPolicy::Require];

    // When: each failure is decided
    // Then: neither policy converts a genuine rendering failure into a skip
    for result in &results {
        for policy in policies {
            assert!(matches!(decide(policy, result), Decision::Fail(_)));
        }
    }
}

#[test]
fn decide_returns_frame_under_every_policy_when_capture_succeeds() {
    // Given: a successful capture and both adapter policies
    let result = Ok(CapturedFrame {
        width: 1,
        height: 1,
        rgba: vec![0, 0, 0, 255],
    });
    let policies = [AdapterPolicy::SkipIfMissing, AdapterPolicy::Require];

    // When: each policy decides the successful result
    // Then: the frame is preserved as evidence
    for policy in policies {
        assert!(matches!(decide(policy, &result), Decision::Frame));
    }
}

#[test]
fn capture_or_skip_returns_none_without_adapter_when_skip_policy() {
    // Given: a headless workbench and the opt-in skip policy
    unsafe { std::env::set_var("EVORCH_REQUIRE_ADAPTER", "0") };
    let mut workbench = workbench();
    workbench.run();

    // When: capture is attempted through the evidence boundary
    let outcome = catch_unwind(AssertUnwindSafe(|| capture_or_skip(&mut workbench)));

    // Then: adapter-free hosts return None and adapter-enabled hosts return evidence
    match outcome.expect("capture_or_skip must not unwind") {
        None => {}
        Some(frame) => {
            assert_eq!((frame.width, frame.height), (640, 360));
            assert_eq!(frame.rgba.len(), 640 * 360 * 4);
        }
    }
}
