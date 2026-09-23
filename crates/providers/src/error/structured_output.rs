use serde_json::Value;

/// Inspect the provider's error, never arbitrary echoed request/schema content.
pub(super) fn is_unsupported(body: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let error = parsed
        .as_ref()
        .map(|value| value.get("error").unwrap_or(value));
    let message = error
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
        .unwrap_or_else(|| if parsed.is_none() { body } else { "" })
        .to_ascii_lowercase();
    let param = error
        .and_then(|value| value.get("param"))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    // Invalid schemas are implementation errors, not evidence of unavailable output control.
    if matches!(
        code.as_str(),
        "invalid_json_schema" | "schema_validation_error"
    ) {
        return false;
    }
    if [
        // Codex response.failed currently flattens its code and message into this text.
        "invalid_json_schema",
        "schema_validation_error",
        "invalid schema",
        "invalid json schema",
        "invalid json_schema",
        "schema is invalid",
        "schema validation",
        "schema must",
        "unsupported schema",
        "unsupported keyword",
        "schema keyword",
        "in context=",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        return false;
    }
    if let Some(param) = &param {
        // A nested schema error or an unrelated parameter must not disable the constraint.
        if !is_format_parameter(param) {
            return false;
        }
    }
    let mentions_format = param.is_some()
        || [
            "response_format",
            "json_schema",
            "structured output",
            "text.format",
            "parameter: 'text'",
            "parameter: \"text\"",
        ]
        .iter()
        .any(|needle| message.contains(needle));
    let unsupported = [
        "not supported",
        "unsupported",
        "does not support",
        "do not support",
        "doesn't support",
        "not implemented",
        "unknown parameter",
        "unrecognized parameter",
        "unrecognized request argument",
    ]
    .iter()
    .any(|needle| message.contains(needle))
        || (param.is_some()
            && matches!(
                code.as_str(),
                "unsupported_parameter" | "unknown_parameter" | "unsupported_value"
            ));
    mentions_format && unsupported
}

fn is_format_parameter(param: &str) -> bool {
    matches!(
        param,
        "response_format"
            | "response_format.type"
            | "response_format.json_schema"
            | "json_schema"
            | "text"
            | "text.format"
            | "text.format.type"
    )
}

#[cfg(test)]
mod tests {
    use crate::ProviderError;
    use serde_json::json;

    #[test]
    fn explicit_unsupported_output_formats_allow_fallback() {
        for body in [
            "Unsupported parameter: 'response_format'".to_owned(),
            "response_format of type json_schema is not supported with this model".into(),
            "This model does not support structured outputs".into(),
            "Unknown parameter: text.format".into(),
            "Unsupported parameter: 'text'".into(),
            "Structured outputs are not implemented".into(),
            json!({"error": {"message": "Unsupported value", "param": "response_format.type", "code": "unsupported_value"}}).to_string(),
            json!({"error": {"message": "Unrecognized request argument supplied: response_format", "param": null}}).to_string(),
            json!({"detail": "text.format is not supported"}).to_string(),
        ] {
            for status in [400, 422, 501] {
                let error = ProviderError::Http { status, body: body.clone() };
                assert!(error.is_structured_output_unsupported(), "{error}");
            }
        }
    }

    #[test]
    fn schema_errors_and_other_http_failures_do_not_allow_fallback() {
        for body in [
            "Invalid schema for response_format 'verdict': required is missing".to_owned(),
            "Invalid JSON schema: unsupported type".into(),
            "json_schema contains an unsupported keyword".into(),
            "Unsupported schema keyword in response_format".into(),
            "Invalid response_format".into(),
            json!({"error": {"message": "oneOf is not supported", "param": "response_format", "code": "invalid_json_schema"}}).to_string(),
            json!({"error": {"message": "In context=(), oneOf is not supported", "param": "text.format"}}).to_string(),
            "Bad request".into(),
            json!({"error": {"message": "unsupported field", "param": "response_format.json_schema.schema.properties.foo"}}).to_string(),
            json!({"error": {"message": "unsupported temperature with response_format", "param": "temperature"}}).to_string(),
            // Echoed request fields cannot make an unrelated rejection look format-specific.
            json!({"error": {"message": "Unsupported model"}, "request": {"response_format": "json_schema"}}).to_string(),
        ] {
            let error = ProviderError::Http { status: 400, body };
            assert!(!error.is_structured_output_unsupported(), "{error}");
        }
        for status in [401, 403, 404, 408, 429, 500, 502, 503] {
            assert!(
                !ProviderError::Http {
                    status,
                    body: "json_schema is not supported".into(),
                }
                .is_structured_output_unsupported()
            );
        }
    }

    #[test]
    fn transport_errors_are_not_format_errors_and_retry_wrappers_preserve_cause() {
        for error in [
            ProviderError::Timeout,
            ProviderError::RateLimited { retry_after: None },
            ProviderError::Request("json_schema is not supported".into()),
            ProviderError::Transport {
                message: "network unavailable".into(),
            },
            ProviderError::InvalidJson {
                detail: "json_schema unsupported".into(),
            },
            ProviderError::InvalidSse {
                detail: "json_schema unsupported".into(),
            },
        ] {
            assert!(!error.is_structured_output_unsupported());
            assert!(
                !ProviderError::RetriesExhausted {
                    attempts: 3,
                    last: Box::new(error)
                }
                .is_structured_output_unsupported()
            );
        }
        assert!(
            ProviderError::RetriesExhausted {
                attempts: 1,
                last: Box::new(ProviderError::Http {
                    status: 400,
                    body: "response_format is not supported".into(),
                }),
            }
            .is_structured_output_unsupported()
        );
    }
}
