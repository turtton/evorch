//! Classification of Connect end-frame errors, including Cursor's own error details.
//!
//! Cursor attaches an `aiserver.v1.ErrorDetails` entry to most rejections: its error
//! enum and retry verdict say far more than the Connect code (a provider outage arrives
//! as `resource_exhausted` or `unavailable`, for example). Field numbers and the
//! status/retry tables follow oh-my-pi `cursor-models.proto` and `providers/cursor.ts`
//! (`classifyCursorStructuredError`, OMP-LICENSE).
use super::wire;
use crate::ProviderError;
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use serde_json::Value;

/// Maps a Connect end-frame error to the error its HTTP equivalent would produce.
///
/// The server already answered, so this is a remote failure of this profile, never a
/// local request error: the router must be able to fall back on it like on an HTTP
/// status. Cursor's error details win over the Connect code. Without them, codes
/// follow the Connect protocol's HTTP mapping, and a code outside it counts as
/// `unknown` (500).
pub(super) fn connect_error(value: &Value) -> ProviderError {
    // Server error text is untrusted and may echo authentication or prompts, so only
    // recognized codes are reported, never the server's own words.
    let (code, status) = value
        .get("code")
        .and_then(Value::as_str)
        .and_then(|code| connect_status(code).map(|status| (code, status)))
        .unwrap_or(("unknown", 500));
    if let Some(detail) = cursor_detail(value) {
        return detail.classify(code);
    }
    match code {
        "resource_exhausted" => ProviderError::RateLimited { retry_after: None },
        "deadline_exceeded" => ProviderError::Timeout,
        _ => ProviderError::Http {
            status,
            body: format!("Cursor Connect request failed (code {code})"),
        },
    }
}

/// The HTTP status the Connect protocol assigns to `code`, or `None` if it is not one.
fn connect_status(code: &str) -> Option<u16> {
    Some(match code {
        "canceled" => 499,
        "unknown" | "internal" | "data_loss" => 500,
        "invalid_argument" | "failed_precondition" | "out_of_range" => 400,
        "deadline_exceeded" => 504,
        "not_found" => 404,
        "already_exists" | "aborted" => 409,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "unimplemented" => 501,
        "unavailable" => 503,
        "unauthenticated" => 401,
        _ => return None,
    })
}

/// `aiserver.v1.ErrorDetails.CursorError`, without the `ERROR_` prefix.
const CURSOR_ERRORS: &[(u64, &str)] = &[
    (0, "UNSPECIFIED"),
    (1, "BAD_API_KEY"),
    (2, "NOT_LOGGED_IN"),
    (3, "INVALID_AUTH_ID"),
    (4, "NOT_HIGH_ENOUGH_PERMISSIONS"),
    (5, "BAD_MODEL_NAME"),
    (6, "USER_NOT_FOUND"),
    (7, "FREE_USER_RATE_LIMIT_EXCEEDED"),
    (8, "PRO_USER_RATE_LIMIT_EXCEEDED"),
    (9, "FREE_USER_USAGE_LIMIT"),
    (10, "PRO_USER_USAGE_LIMIT"),
    (11, "AUTH_TOKEN_NOT_FOUND"),
    (12, "AUTH_TOKEN_EXPIRED"),
    (13, "OPENAI"),
    (14, "OPENAI_RATE_LIMIT_EXCEEDED"),
    (18, "AGENT_REQUIRES_LOGIN"),
    (20, "MAX_TOKENS"),
    (21, "USER_ABORTED_REQUEST"),
    (22, "GENERIC_RATE_LIMIT_EXCEEDED"),
    (23, "PRO_USER_ONLY"),
    (25, "TIMEOUT"),
    (28, "GPT_4_VISION_PREVIEW_RATE_LIMIT"),
    (29, "CUSTOM_MESSAGE"),
    (30, "OUTDATED_CLIENT"),
    (31, "CLAUDE_IMAGE_TOO_LARGE"),
    (33, "FILE_NOT_FOUND"),
    (34, "API_KEY_RATE_LIMIT"),
    (35, "DEBOUNCED"),
    (36, "BAD_REQUEST"),
    (37, "REPOSITORY_SERVICE_REPOSITORY_IS_NOT_INITIALIZED"),
    (38, "UNAUTHORIZED"),
    (39, "NOT_FOUND"),
    (40, "DEPRECATED"),
    (41, "RESOURCE_EXHAUSTED"),
    (42, "BAD_USER_API_KEY"),
    (43, "CONVERSATION_TOO_LONG"),
    (44, "USAGE_PRICING_REQUIRED"),
    (45, "USAGE_PRICING_REQUIRED_CHANGEABLE"),
    (46, "GITHUB_NO_USER_CREDENTIALS"),
    (47, "GITHUB_USER_NO_ACCESS"),
    (48, "GITHUB_APP_NO_ACCESS"),
    (49, "GITHUB_MULTIPLE_OWNERS"),
    (50, "RATE_LIMITED"),
    (51, "RATE_LIMITED_CHANGEABLE"),
    (52, "CUSTOM"),
    (53, "HOOKS_BLOCKED"),
    (54, "SUSPICIOUS_USAGE_BLOCKED"),
    (55, "EXTENSION_HOST_TIMEOUT"),
    (56, "NETWORK_ERROR"),
    (57, "PROVIDER_ERROR"),
    (58, "MODEL_BLOCKED"),
    (59, "INTERNAL"),
    (60, "MAX_MODE_REQUIRED"),
    (61, "MODEL_NO_LONGER_SUPPORTED"),
    (62, "PRICING_WARNING"),
    (63, "SLOW_POOL"),
    (64, "UNSUPPORTED_REGION"),
    (65, "ACCOUNT_CLOSED"),
];

/// Errors Cursor treats as retryable when its detail carries no verdict.
const RETRYABLE: &[u64] = &[13, 25, 55, 56, 57, 59, 63];

/// The HTTP status a Cursor error stands for, when it names one.
fn cursor_status(error: u64) -> Option<u16> {
    Some(match error {
        1 | 2 | 3 | 11 | 12 | 18 | 38 | 42 | 65 => 401,
        4 | 23 | 46 | 47 | 48 | 49 | 54 | 58 | 64 => 403,
        5 | 6 | 33 | 39 | 61 => 404,
        20 | 43 => 413,
        7 | 8 | 9 | 10 | 14 | 22 | 28 | 34 | 35 | 41 | 44 | 45 | 50 | 51 => 429,
        _ => return None,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct CursorDetail {
    error: Option<u64>,
    retryable: Option<bool>,
}

impl CursorDetail {
    fn classify(&self, code: &str) -> ProviderError {
        let name = self.error.map_or_else(
            || "unrecognized".to_owned(),
            |error| {
                CURSOR_ERRORS
                    .iter()
                    .find(|(known, _)| *known == error)
                    .map_or_else(|| format!("ERROR_{error}"), |(_, name)| (*name).to_owned())
            },
        );
        let body = format!(
            "Cursor Connect request failed (code {code}, cursor {name}{})",
            match self.retryable {
                Some(true) => ", retryable",
                Some(false) => ", not retryable",
                None => "",
            }
        );
        let status = match self.error {
            // The user or client cancelled; never a reason to try another profile.
            Some(21) => 499,
            Some(error) => cursor_status(error).unwrap_or_else(|| {
                if self.retryable.unwrap_or_else(|| RETRYABLE.contains(&error)) {
                    503
                } else {
                    400
                }
            }),
            None if self.retryable == Some(true) => 503,
            None => connect_status(code).unwrap_or(500),
        };
        if status == 429 {
            ProviderError::RateLimited { retry_after: None }
        } else {
            ProviderError::Http { status, body }
        }
    }
}

/// Reads Cursor's `aiserver.v1.ErrorDetails` from the Connect error's `details`, either
/// from its binary `value` or, when only that is present, from its JSON `debug` form.
fn cursor_detail(value: &Value) -> Option<CursorDetail> {
    value
        .get("details")?
        .as_array()?
        .iter()
        .filter(|entry| {
            matches!(
                entry.get("type").and_then(Value::as_str),
                Some("aiserver.v1.ErrorDetails" | "type.googleapis.com/aiserver.v1.ErrorDetails")
            )
        })
        .find_map(|entry| {
            entry
                .get("value")
                .and_then(Value::as_str)
                .and_then(decode_binary)
                .or_else(|| entry.get("debug").and_then(decode_debug))
        })
}

fn decode_binary(encoded: &str) -> Option<CursorDetail> {
    // Connect JSON carries bytes as standard base64, with or without padding.
    let bytes = STANDARD_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .ok()
        .filter(|bytes| !bytes.is_empty())?;
    let error = wire::integer(&bytes, 1).ok()?;
    let retryable = match wire::nested(&bytes, 2).ok()? {
        Some(details) => wire::integer(details, 4).ok()?.map(|flag| flag != 0),
        None => None,
    };
    // proto3 omits a zero enum, so an absent error is UNSPECIFIED.
    Some(CursorDetail {
        error: Some(error.unwrap_or(0)),
        retryable,
    })
}

fn decode_debug(debug: &Value) -> Option<CursorDetail> {
    let debug = debug.as_object()?;
    let error = debug.get("error").and_then(Value::as_str).and_then(|name| {
        let name = name.strip_prefix("ERROR_").unwrap_or(name);
        CURSOR_ERRORS
            .iter()
            .find(|(_, known)| *known == name)
            .map(|(error, _)| *error)
    });
    let retryable = debug
        .get("details")
        .and_then(|details| details.get("isRetryable"))
        .and_then(Value::as_bool);
    (error.is_some() || retryable.is_some()).then_some(CursorDetail { error, retryable })
}
