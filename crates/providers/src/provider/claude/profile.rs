//! Pinned Claude CLI transport identity and model-validated reasoning controls.

use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::{Value, json};
use std::borrow::Cow;

use crate::ProviderError;

pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.295";
pub(crate) const CLAUDE_SDK_VERSION: &str = "0.128.0";
pub(crate) const CLAUDE_RUNTIME_VERSION: &str = "v26.3.0";
pub const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.295 (external, cli)";
pub const CLAUDE_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub const CLAUDE_INFERENCE_BETAS: &str = concat!(
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,",
    "thinking-token-count-2026-05-13,context-management-2025-06-27,",
    "prompt-caching-scope-2026-01-05,",
    "extended-cache-ttl-2025-04-11"
);

/// Updating this pinned profile is a release decision, never a mid-run mutation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ClaudeWireProfile;

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct ClaudeReasoningControls {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
}

impl ClaudeReasoningControls {
    pub fn is_enabled(&self) -> bool {
        self.thinking
            .as_ref()
            .and_then(|thinking| thinking["type"].as_str())
            .is_some_and(|kind| kind != "disabled")
    }
}

impl ClaudeWireProfile {
    pub fn requires_default_temperature(&self, model: &str) -> bool {
        model_mode(model).is_some_and(|mode| mode.supports_xhigh() || mode.requires_thinking())
    }

    /// Authentication is deliberately supplied by the caller, not this profile.
    pub fn headers(
        &self,
        session_id: &str,
        controls: &ClaudeReasoningControls,
    ) -> Result<HeaderMap, ProviderError> {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("accept", "application/json"),
            ("content-type", "application/json"),
            ("user-agent", CLAUDE_USER_AGENT),
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", CLAUDE_SDK_VERSION),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", CLAUDE_RUNTIME_VERSION),
            ("x-stainless-timeout", "600"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
            ("connection", "keep-alive"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        for (name, value) in [
            ("x-stainless-arch", stainless_arch(std::env::consts::ARCH)),
            ("x-stainless-os", stainless_os(std::env::consts::OS)),
        ] {
            headers.insert(
                name,
                HeaderValue::from_str(&value).map_err(|_| {
                    ProviderError::Request("Claude platform identity is invalid".into())
                })?,
            );
        }
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_str(session_id).map_err(|_| {
                ProviderError::Request("Claude session identity is not a valid HTTP header".into())
            })?,
        );
        let mut betas: Vec<_> = CLAUDE_INFERENCE_BETAS
            .split(',')
            .filter(|beta| *beta != "extended-cache-ttl-2025-04-11")
            .collect();
        if controls
            .output_config
            .as_ref()
            .is_some_and(|value| value["effort"].is_string())
        {
            betas.push("effort-2025-11-24");
        }
        if controls
            .thinking
            .as_ref()
            .is_some_and(|value| value["display"] == "updates")
        {
            betas.push("thinking-display-updates-2026-08-18");
        }
        betas.push("extended-cache-ttl-2025-04-11");
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_str(&betas.join(",")).map_err(|_| {
                ProviderError::Request("Claude inference beta profile is invalid".into())
            })?,
        );
        Ok(headers)
    }

    /// `None` preserves an unspecified configuration; `Some("none")` disables thinking.
    /// Effort/budget changes are explicit cache boundaries and must stay stable per run.
    pub fn reasoning(
        &self,
        model: &str,
        max_tokens: u64,
        effort: Option<&str>,
    ) -> Result<ClaudeReasoningControls, ProviderError> {
        let Some(effort) = effort else {
            return Ok(ClaudeReasoningControls::default());
        };
        let mode = model_mode(model).ok_or_else(|| {
            ProviderError::Request(format!(
                "Claude OAuth reasoning controls are not supported for model {model}"
            ))
        })?;
        if effort == "none" {
            if mode.requires_thinking() {
                return Err(ProviderError::Request(format!(
                    "Claude model {model} requires thinking; select low effort instead"
                )));
            }
            return Ok(ClaudeReasoningControls {
                thinking: Some(json!({"type": "disabled"})),
                ..Default::default()
            });
        }
        let budget = match effort {
            "low" => 1024,
            "medium" => 4096,
            "high" => 16384,
            "xhigh" if mode.supports_xhigh() => 0,
            "max" if mode.is_adaptive() => 0,
            _ => {
                return Err(ProviderError::Request(format!(
                    "Claude OAuth reasoning effort {effort:?} is not supported for model {model}"
                )));
            }
        };
        let thinking = if mode.is_adaptive() {
            json!({"type": "adaptive", "display": "updates"})
        } else {
            if max_tokens <= 1024 {
                return Err(ProviderError::Request(
                    "Claude extended thinking requires max_tokens greater than 1024".into(),
                ));
            }
            json!({"type": "enabled", "budget_tokens": budget.min(max_tokens - 1)})
        };
        Ok(ClaudeReasoningControls {
            thinking: Some(thinking),
            output_config: (mode.is_adaptive() || mode == ModelMode::ManualEffort)
                .then(|| json!({"effort": effort})),
            context_management: (mode != ModelMode::ManualLegacy)
                .then(|| json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]})),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelMode {
    Adaptive,
    AdaptiveXhigh,
    AdaptiveRequired,
    AdaptiveRequiredXhigh,
    Manual,
    ManualEffort,
    ManualLegacy,
}

impl ModelMode {
    fn is_adaptive(self) -> bool {
        matches!(
            self,
            Self::Adaptive
                | Self::AdaptiveXhigh
                | Self::AdaptiveRequired
                | Self::AdaptiveRequiredXhigh
        )
    }

    fn supports_xhigh(self) -> bool {
        matches!(self, Self::AdaptiveXhigh | Self::AdaptiveRequiredXhigh)
    }

    fn requires_thinking(self) -> bool {
        matches!(self, Self::AdaptiveRequired | Self::AdaptiveRequiredXhigh)
    }
}

fn model_mode(model: &str) -> Option<ModelMode> {
    for (name, mode) in [
        ("claude-sonnet-4-6", ModelMode::Adaptive),
        ("claude-opus-4-6", ModelMode::Adaptive),
        ("claude-opus-4-7", ModelMode::AdaptiveXhigh),
        ("claude-opus-4-8", ModelMode::AdaptiveXhigh),
        ("claude-opus-5", ModelMode::AdaptiveXhigh),
        ("claude-sonnet-5", ModelMode::AdaptiveXhigh),
        ("claude-haiku-5-5", ModelMode::AdaptiveXhigh),
        ("claude-opus-5-5", ModelMode::AdaptiveRequiredXhigh),
        ("claude-sonnet-5-5", ModelMode::AdaptiveRequiredXhigh),
        ("claude-fable-5", ModelMode::AdaptiveRequiredXhigh),
        ("claude-fable-5-1", ModelMode::AdaptiveRequiredXhigh),
        ("claude-mythos-5", ModelMode::AdaptiveRequiredXhigh),
        ("claude-mythos-5-1", ModelMode::AdaptiveRequiredXhigh),
        ("claude-mythos-preview", ModelMode::AdaptiveRequired),
        ("claude-opus-4-5", ModelMode::ManualEffort),
        ("claude-sonnet-4-5", ModelMode::Manual),
        ("claude-haiku-4-5", ModelMode::Manual),
        ("claude-opus-4-1", ModelMode::Manual),
        ("claude-opus-4-0", ModelMode::Manual),
        ("claude-sonnet-4-0", ModelMode::Manual),
        ("claude-opus-4", ModelMode::Manual),
        ("claude-sonnet-4", ModelMode::Manual),
        ("claude-3-7-sonnet", ModelMode::ManualLegacy),
    ] {
        if model == name
            || model.strip_prefix(name).is_some_and(|suffix| {
                suffix.strip_prefix('-').is_some_and(|date| {
                    date.len() == 8 && date.bytes().all(|digit| digit.is_ascii_digit())
                })
            })
        {
            return Some(mode);
        }
    }
    None
}

fn stainless_arch(arch: &'static str) -> Cow<'static, str> {
    match arch {
        "x86_64" => "x64".into(),
        "aarch64" => "arm64".into(),
        "x86" => "x86".into(),
        _ => format!("other::{arch}").into(),
    }
}

fn stainless_os(os: &'static str) -> Cow<'static, str> {
    match os {
        "linux" => "Linux".into(),
        "macos" => "MacOS".into(),
        "windows" => "Windows".into(),
        "freebsd" => "FreeBSD".into(),
        "openbsd" => "OpenBSD".into(),
        _ => format!("Other::{os}").into(),
    }
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
