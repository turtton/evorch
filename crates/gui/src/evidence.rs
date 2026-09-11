//! Offscreen evidence capture policy.

use crate::headless::{CapturedFrame, HeadlessWorkbench, OffscreenError};
use crate::model::tasks::AgentRunSource;

/// Whether CI requires a usable offscreen GPU adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterPolicy {
    /// Adapter absence fails evidence capture.
    Require,
    /// Adapter absence skips evidence capture.
    SkipIfMissing,
}

impl AdapterPolicy {
    /// Parses the `EVORCH_REQUIRE_ADAPTER` environment value.
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            Some("1") => Self::Require,
            None | Some("0") | Some("") | Some(_) => Self::SkipIfMissing,
        }
    }

    /// Reads the adapter policy from `EVORCH_REQUIRE_ADAPTER`.
    pub fn from_env() -> Self {
        Self::from_value(std::env::var("EVORCH_REQUIRE_ADAPTER").ok().as_deref())
    }
}

/// The action to take for an offscreen render result.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Retain the captured frame.
    Frame,
    /// Omit evidence because no adapter is available.
    Skip(String),
    /// Stop because evidence capture failed.
    Fail(String),
}

/// Decides how offscreen evidence failures are handled.
pub fn decide(policy: AdapterPolicy, result: &Result<CapturedFrame, OffscreenError>) -> Decision {
    match result {
        Ok(_) => Decision::Frame,
        Err(OffscreenError::AdapterUnavailable(reason)) => match policy {
            AdapterPolicy::Require => Decision::Fail(format!(
                "offscreen adapter is required; set EVORCH_REQUIRE_ADAPTER=0 to permit a skip: {reason}"
            )),
            AdapterPolicy::SkipIfMissing => Decision::Skip(reason.clone()),
        },
        Err(OffscreenError::Render(reason)) => {
            Decision::Fail(format!("offscreen render failed: {reason}"))
        }
        Err(OffscreenError::Encode(reason)) => {
            Decision::Fail(format!("offscreen PNG encoding failed: {reason}"))
        }
    }
}

/// Captures evidence or handles adapter absence according to the environment policy.
pub fn capture_or_skip<S: AgentRunSource + 'static>(
    workbench: &mut HeadlessWorkbench<S>,
) -> Option<CapturedFrame> {
    let result = workbench.capture();
    match (decide(AdapterPolicy::from_env(), &result), result) {
        (Decision::Frame, Ok(frame)) => Some(frame),
        (Decision::Skip(message), Err(OffscreenError::AdapterUnavailable(_))) => {
            eprintln!("offscreen evidence skipped: {message}");
            None
        }
        (Decision::Skip(message), Err(OffscreenError::Render(error))) => {
            panic!(
                "offscreen evidence decision was inconsistent with render error: {message}; {error}"
            )
        }
        (Decision::Skip(message), Err(OffscreenError::Encode(error))) => {
            panic!(
                "offscreen evidence decision was inconsistent with encoding error: {message}; {error}"
            )
        }
        (Decision::Fail(message), Err(_)) => panic!("{message}"),
        (Decision::Frame, Err(error)) => {
            panic!("offscreen evidence decision was inconsistent with capture error: {error}")
        }
        (Decision::Skip(message), Ok(_)) => {
            panic!("offscreen evidence decision was inconsistent with captured frame: {message}")
        }
        (Decision::Fail(message), Ok(_)) => {
            panic!("offscreen evidence decision was inconsistent with captured frame: {message}")
        }
    }
}
