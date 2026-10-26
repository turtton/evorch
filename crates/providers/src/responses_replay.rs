//! Non-wire identities and final guards for Responses encrypted reasoning.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ChatRequest, ContentBlock, ProviderError, Role, ServiceTier};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesDialect {
    OpenAi,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesSpeed {
    Standard,
    Fast,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResponsesAccount {
    Unavailable,
    Codex { identity: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedResponsesScope {
    pub dialect: ResponsesDialect,
    pub profile: String,
    pub route_model: String,
    pub wire_model: String,
    pub speed: ResponsesSpeed,
    pub service_tier: Option<ServiceTier>,
    pub endpoint_fingerprint: String,
    pub account: ResponsesAccount,
}

pub type ResponsesReplayOrigin = ResolvedResponsesScope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsesReplayContext {
    pub route_model: String,
    pub speed: ResponsesSpeed,
    pub expected_scope: Option<ResolvedResponsesScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponsesOutputPosition {
    pub output_index: u64,
    pub item_id: String,
}

/// Components excluded from a non-secret identity must not select another route.
pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), ProviderError> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| ProviderError::Request("invalid Responses endpoint".into()))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::Request(
            "Responses endpoint must be an HTTP(S) URL without credentials, query, or fragment"
                .into(),
        ));
    }
    Ok(())
}

impl ResolvedResponsesScope {
    /// Resolve from the actual endpoint and the same account lease used for HTTP.
    pub fn resolve(
        request: &ChatRequest,
        dialect: ResponsesDialect,
        profile: Option<&str>,
        endpoint: &str,
        account: ResponsesAccount,
    ) -> Self {
        let context = request.responses_replay.as_ref();
        let endpoint = reqwest::Url::parse(endpoint).map(|mut url| {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        });
        Self {
            dialect,
            profile: profile.unwrap_or_default().to_owned(),
            route_model: context.map_or_else(|| request.model.clone(), |c| c.route_model.clone()),
            wire_model: request.model.clone(),
            speed: context.map_or(ResponsesSpeed::Standard, |c| c.speed),
            service_tier: request.service_tier,
            endpoint_fingerprint: format!(
                "{:x}",
                Sha256::digest(endpoint.unwrap_or_default().as_bytes())
            ),
            account,
        }
    }
}

/// Only complete, self-contained items are replayable. Never repair ciphertext.
pub fn is_replayable_item(item: &serde_json::Value) -> bool {
    item.is_object()
        && item["type"] == "reasoning"
        && item["id"].as_str().is_some_and(|s| !s.is_empty())
        && item["summary"].is_array()
        && item["encrypted_content"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && (item.get("status").is_none() || item["status"] == "completed")
}

/// Runtime must additionally apply its monotonic raw-message replay floor.
pub fn eligible(block: &ContentBlock, role: Role, scope: &ResolvedResponsesScope) -> bool {
    matches!(block, ContentBlock::ResponsesReasoning { item, origin }
        if role == Role::Assistant && origin == scope && is_replayable_item(item))
}

/// A preparation change is control flow, not a retryable transport failure.
pub fn validate_request_scope(
    request: &ChatRequest,
    actual: &ResolvedResponsesScope,
) -> Result<(), ProviderError> {
    let expected = request
        .responses_replay
        .as_ref()
        .and_then(|c| c.expected_scope.as_ref());
    let has_opaque = request.messages.iter().any(|m| {
        m.role == Role::Assistant
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ResponsesReasoning { .. }))
    });
    if expected.is_some_and(|e| e != actual)
        || (has_opaque && expected.is_none())
        || request.messages.iter().any(|m| {
            m.role == Role::Assistant && m.content.iter().any(|b|
            matches!(b, ContentBlock::ResponsesReasoning { origin, .. } if origin != actual))
        })
    {
        return Err(ProviderError::ResponsesScopeChanged {
            actual: Box::new(actual.clone()),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scopes_compare_every_identity_and_do_not_depend_on_tokens() {
        let mut request: ChatRequest =
            serde_json::from_value(json!({"model":"m","messages":[]})).unwrap();
        request.service_tier = Some(ServiceTier::Priority);
        request.responses_replay = Some(ResponsesReplayContext {
            route_model: "m+fast".into(),
            speed: ResponsesSpeed::Fast,
            expected_scope: None,
        });
        let a = ResolvedResponsesScope::resolve(
            &request,
            ResponsesDialect::Codex,
            Some("work"),
            "https://example.test/responses",
            ResponsesAccount::Codex {
                identity: "account-A".into(),
            },
        );
        assert_eq!(a.wire_model, "m");
        assert_eq!(a.route_model, "m+fast");
        assert_eq!(a.speed, ResponsesSpeed::Fast);
        assert_eq!(a.service_tier, Some(ServiceTier::Priority));
        let mut variants = vec![];
        let mut b = a.clone();
        b.dialect = ResponsesDialect::OpenAi;
        variants.push(b);
        let mut b = a.clone();
        b.profile.push('2');
        variants.push(b);
        let mut b = a.clone();
        b.route_model = "m".into();
        variants.push(b);
        let mut b = a.clone();
        b.wire_model = "m2".into();
        variants.push(b);
        let mut b = a.clone();
        b.speed = ResponsesSpeed::Standard;
        variants.push(b);
        let mut b = a.clone();
        b.service_tier = None;
        variants.push(b);
        let mut b = a.clone();
        b.endpoint_fingerprint.push('2');
        variants.push(b);
        let mut b = a.clone();
        b.account = ResponsesAccount::Codex {
            identity: "account-B".into(),
        };
        variants.push(b);
        for b in variants {
            request.responses_replay.as_mut().unwrap().expected_scope = Some(b);
            assert!(matches!(
                validate_request_scope(&request, &a),
                Err(ProviderError::ResponsesScopeChanged { .. })
            ));
        }
        request.responses_replay.as_mut().unwrap().expected_scope = Some(a.clone());
        assert!(validate_request_scope(&request, &a).is_ok());
        assert_eq!(
            serde_json::from_value::<ResolvedResponsesScope>(serde_json::to_value(&a).unwrap())
                .unwrap(),
            a
        );
        assert!(
            serde_json::to_value(request)
                .unwrap()
                .get("responses_replay")
                .is_none()
        );
    }
}
