use super::*;
use providers::ProviderError;
use providers::provider::codex::tokens::TokenBundle;
use sandbox::CredentialStore;

#[test]
fn requests_sign_in_when_authorization_is_rejected() {
    // Given
    for status in [401, 403] {
        let error = ProviderError::Http {
            status,
            body: "private detail".into(),
        };
        // When
        let message = map_fetch_error(&error);
        // Then
        assert_eq!(message, "Codex authorization rejected; Sign in again");
    }
}

#[test]
fn preserves_status_and_body_when_http_fetch_fails() {
    // Given
    let error = ProviderError::Http {
        status: 502,
        body: "catalog unavailable".into(),
    };
    // When
    let message = map_fetch_error(&error);
    // Then
    assert_eq!(
        message,
        "Could not fetch Codex models: HTTP 502: catalog unavailable"
    );
}

#[test]
fn truncates_body_safely_when_http_detail_exceeds_300_characters() {
    // Given
    for body in ["a".repeat(301), "界🦀".repeat(151)] {
        let error = ProviderError::Http {
            status: 500,
            body: body.clone(),
        };
        // When
        let message = map_fetch_error(&error);
        // Then
        let detail = message
            .strip_prefix("Could not fetch Codex models: HTTP 500: ")
            .unwrap();
        assert_eq!(detail.chars().count(), 300);
        if body.starts_with('a') {
            assert!(detail.chars().all(|character| character == 'a'));
        } else {
            assert_eq!(detail.matches('界').count(), 150);
            assert_eq!(detail.matches('🦀').count(), 150);
            assert!(detail.ends_with("界🦀"));
        }
    }
}

#[test]
fn preserves_body_when_at_or_below_character_limit() {
    // Given
    for body in [String::new(), "界".repeat(299), "🦀".repeat(300)] {
        let error = ProviderError::Http {
            status: 400,
            body: body.clone(),
        };
        // When
        let message = map_fetch_error(&error);
        // Then
        assert_eq!(
            message,
            format!("Could not fetch Codex models: HTTP 400: {body}")
        );
    }
}

#[test]
fn preserves_detail_when_failure_is_not_http() {
    // Given
    for error in [
        ProviderError::Timeout,
        ProviderError::InvalidJson {
            detail: "missing models".into(),
        },
        ProviderError::InvalidSse {
            detail: "bad event".into(),
        },
        ProviderError::Request("connection reset".into()),
        ProviderError::Transport {
            message: "DNS failed".into(),
        },
        ProviderError::RateLimited { retry_after: None },
        ProviderError::RetriesExhausted {
            attempts: 2,
            last: Box::new(ProviderError::Timeout),
        },
    ] {
        // When
        let message = map_fetch_error(&error);
        // Then
        assert_eq!(message, format!("Could not fetch Codex models: {error}"));
    }
}

#[test]
fn rejects_id_token_when_account_claim_is_invalid() {
    // Given
    for id_token in [
        "malformed".to_owned(),
        format!(
            "header.{}.sig",
            URL_SAFE_NO_PAD.encode(r#"{"exp":18446744073709551615}"#)
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(sandbox::FileCredentialStore::open(dir.path()).unwrap());
        let bundle = TokenBundle {
            access_token: format!(
                "header.{}.sig",
                URL_SAFE_NO_PAD.encode(r#"{"exp":18446744073709551615}"#)
            ),
            refresh_token: "unused".into(),
            id_token,
        };
        store
            .set(
                "profile",
                &sandbox::Secret::from(serde_json::to_string(&bundle).unwrap()),
            )
            .unwrap();
        // When
        let result = access_token(Some(store), "profile".into());
        // Then
        assert!(
            matches!(result, Err(message) if message == "Invalid Codex ID token; Sign in again")
        );
    }
}

#[test]
fn expands_fast_capable_models_into_base_and_fast_rows() {
    // Given
    let models = vec![
        providers::CodexModelInfo {
            slug: "gpt-a".into(),
            supports_fast: true,
        },
        providers::CodexModelInfo {
            slug: "gpt-b".into(),
            supports_fast: false,
        },
        providers::CodexModelInfo {
            slug: "gpt-c".into(),
            supports_fast: true,
        },
    ];
    // When
    let ids = expand_fetched_models(models);
    // Then: fast 対応は通常版の直後に fast 版が並び、非対応は単独行のまま
    assert_eq!(ids, ["gpt-a", "gpt-a+fast", "gpt-b", "gpt-c", "gpt-c+fast"]);
}

#[test]
fn labels_fast_variant_with_marker_and_keeps_standard_ids() {
    // Given / When / Then
    assert_eq!(model_display_label("gpt-a+fast"), "gpt-a (fast)");
    assert_eq!(model_display_label("gpt-a"), "gpt-a");
    assert_eq!(model_display_label("+fast"), "+fast");
}

#[test]
fn surfaces_hint_when_backend_returns_zero_models() {
    // Given
    let mut settings = super::super::ProviderSettingsModel::default();
    settings.add(super::super::ProviderKind::CodexSubscription);
    let editor = settings.codex_mut().unwrap();
    let configured = editor.models.clone();
    let (tx, rx) = mpsc::channel();
    tx.send(Ok(CodexFetchedModels {
        models: Vec::new(),
        version: providers::CodexCatalogVersion {
            version: "0.157.0".into(),
            warning: None,
        },
    }))
    .unwrap();
    editor.fetch.models_rx = Some(rx);
    // When
    editor.poll_models();
    // Then
    assert!(
        matches!(&editor.fetch.models_fetch_state, ModelsFetchState::Failed(message)
        if message.contains("0 models") && message.contains("0.157.0"))
    );
    assert_eq!(editor.models, configured);
    assert!(editor.fetch.available_models.is_none());
}
