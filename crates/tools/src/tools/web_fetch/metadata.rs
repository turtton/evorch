use crate::network_guard::NetworkGuardError;
use crate::tools::web_fetch::OutputFormat;
use serde::Serialize;

pub(crate) const TRUNCATION_HINT: &str = "Output truncated to 50KB (51200 bytes). Pass a `selector` argument to narrow extraction to the relevant section, or refine the URL to a more specific page.";

#[derive(Debug, Serialize)]
pub(crate) struct WebFetchMetadata {
    pub(crate) url: String,
    pub(crate) final_url: String,
    pub(crate) status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content_length: Option<u64>,
    pub(crate) decompressed_bytes: usize,
    pub(crate) redirect_count: usize,
    pub(crate) redirect_blocked: bool,
    #[serde(serialize_with = "serialize_format")]
    pub(crate) format: OutputFormat,
    pub(crate) extraction_method: String,
    pub(crate) truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) original_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) truncation_hint: Option<&'static str>,
}

impl WebFetchMetadata {
    pub(crate) fn to_detail(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("metadata serialization cannot fail")
    }
}

fn serialize_format<S>(format: &OutputFormat, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let value = match format {
        OutputFormat::Text => "text",
        OutputFormat::Markdown => "markdown",
        OutputFormat::Html => "html",
    };
    serializer.serialize_str(value)
}

pub(crate) fn error_metadata(url: &str, error: &NetworkGuardError) -> serde_json::Value {
    let mut metadata = serde_json::json!({ "url": url });
    let error_kind = match error {
        NetworkGuardError::InvalidUrl(_) => "invalid_url",
        NetworkGuardError::NotHttpScheme { .. } | NetworkGuardError::NotHttpsAfterUpgrade => {
            "not_https"
        }
        NetworkGuardError::MissingHost => "missing_host",
        NetworkGuardError::DnsResolverInitialization(_)
        | NetworkGuardError::DnsResolutionFailed { .. } => "dns_error",
        NetworkGuardError::BlockedIp { .. } => "blocked_ip",
        NetworkGuardError::RedirectBlocked { .. } => "redirect_blocked",
        NetworkGuardError::HttpsConnectFailed(_) => "connect_failed",
        NetworkGuardError::TooManyRedirects => "too_many_redirects",
        NetworkGuardError::RedirectOnPost { .. } => "redirect_on_post",
        NetworkGuardError::RedirectLocationInvalid(_) => "redirect_location_invalid",
        NetworkGuardError::ResponseTooLarge { check, limit } => {
            metadata["size_check"] = serde_json::Value::String((*check).to_owned());
            metadata["limit_bytes"] = serde_json::json!(*limit);
            "response_too_large"
        }
        NetworkGuardError::DecompressionFailed(_) => "decompression_failed",
        NetworkGuardError::Http(error) if error.is_timeout() => "timeout",
        NetworkGuardError::Http(_) => "http",
    };
    metadata["error_kind"] = serde_json::Value::String(error_kind.to_owned());
    if matches!(error, NetworkGuardError::RedirectBlocked { .. }) {
        metadata["redirect_blocked"] = serde_json::Value::Bool(true);
    }
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_guard::NetworkGuardError;
    use crate::tools::web_fetch::OutputFormat;
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn metadata_serializes_full_success_shape() {
        let metadata = WebFetchMetadata {
            url: "https://example.com".to_owned(),
            final_url: "https://example.com/final".to_owned(),
            status_code: 200,
            content_length: Some(12_345),
            decompressed_bytes: 23_456,
            redirect_count: 1,
            redirect_blocked: false,
            format: OutputFormat::Text,
            extraction_method: "selector".to_owned(),
            truncated: true,
            original_bytes: Some(81_234),
            truncation_hint: Some(TRUNCATION_HINT),
        };
        assert_eq!(
            metadata.to_detail(),
            json!({
                "url": "https://example.com", "final_url": "https://example.com/final",
                "status_code": 200, "content_length": 12345, "decompressed_bytes": 23456,
                "redirect_count": 1, "redirect_blocked": false, "format": "text",
                "extraction_method": "selector", "truncated": true, "original_bytes": 81234,
                "truncation_hint": "Output truncated to 50KB (51200 bytes). Pass a `selector` argument to narrow extraction to the relevant section, or refine the URL to a more specific page."
            })
        );
    }

    #[test]
    fn metadata_omits_absent_optional_fields() {
        let value = WebFetchMetadata {
            url: "https://example.com".to_owned(),
            final_url: "https://example.com".to_owned(),
            status_code: 200,
            content_length: None,
            decompressed_bytes: 1,
            redirect_count: 0,
            redirect_blocked: false,
            format: OutputFormat::Html,
            extraction_method: "raw_html".to_owned(),
            truncated: false,
            original_bytes: None,
            truncation_hint: None,
        }
        .to_detail();
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("content_length")
        );
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("original_bytes")
        );
        assert!(
            !value
                .as_object()
                .expect("object")
                .contains_key("truncation_hint")
        );
    }

    #[test]
    fn too_large_error_metadata_carries_check_and_limit() {
        let error = NetworkGuardError::ResponseTooLarge {
            check: "Content-Length",
            limit: 5_242_880,
        };
        assert_eq!(
            error_metadata("https://example.com", &error),
            json!({
                "url": "https://example.com", "error_kind": "response_too_large",
                "size_check": "Content-Length", "limit_bytes": 5_242_880
            })
        );
    }

    #[test]
    fn redirect_blocked_error_metadata_flags_redirect() {
        let error = NetworkGuardError::RedirectBlocked {
            addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        assert_eq!(
            error_metadata("https://example.com", &error),
            json!({
                "url": "https://example.com", "error_kind": "redirect_blocked", "redirect_blocked": true
            })
        );
    }

    #[test]
    fn exhaustiveness_bonus_covers_blocked_ip_and_redirect_limit() {
        assert_eq!(
            error_metadata(
                "u",
                &NetworkGuardError::BlockedIp {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST)
                }
            ),
            json!({"url":"u","error_kind":"blocked_ip"})
        );
        assert_eq!(
            error_metadata("u", &NetworkGuardError::TooManyRedirects),
            json!({"url":"u","error_kind":"too_many_redirects"})
        );
    }
}
