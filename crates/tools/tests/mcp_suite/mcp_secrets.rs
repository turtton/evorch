use config::mcp::{
    McpServerDefinition, McpTransportDefinition, SourceRevision, Template, TemplatePart,
    ValueSource,
};
use sandbox::{CredentialError, CredentialStore, Secret};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tempfile::tempdir;
use tools::mcp::{
    McpError, McpErrorKind,
    approval::{ApprovalLedger, inspect_stdio},
    secrets::{SecretResolver, credential_key},
    types::*,
};
#[derive(Default)]
struct MemoryStore(Mutex<BTreeMap<String, Secret>>);
impl CredentialStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        self.0.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}
fn scope() -> ApprovalScope {
    ApprovalScope {
        canonical_root: "/project".into(),
        server_id: "server".into(),
    }
}
fn literal(value: &str) -> Template {
    Template(vec![TemplatePart::Literal {
        value: value.into(),
    }])
}
fn env(name: &str, default: Option<&str>) -> Template {
    Template(vec![TemplatePart::Env {
        name: name.into(),
        default: default.map(Into::into),
    }])
}
#[test]
fn unavailable_keychain_does_not_disable_literal_or_env() {
    for reason in [
        SecretUnavailableReason::Unavailable,
        SecretUnavailableReason::Locked,
        SecretUnavailableReason::Denied,
        SecretUnavailableReason::Missing,
    ] {
        let resolver = SecretResolver::new(McpCredentialBackend::Unavailable(reason));
        for (template, expected) in [
            (literal("plain"), "plain"),
            (env("TOKEN", None), "from-env"),
            (env("EMPTY", Some("default")), "default"),
            (env("MISSING", Some("default")), "default"),
        ] {
            let actual = resolver
                .resolve(
                    &scope(),
                    &ValueSource::Template { template },
                    &|name| match name {
                        "TOKEN" => Some("from-env".into()),
                        "EMPTY" => Some(String::new()),
                        _ => None,
                    },
                )
                .unwrap();
            assert_eq!(actual.expose(), expected);
        }
        assert_eq!(
            resolver.resolve_classified(
                &scope(),
                &ValueSource::Keychain {
                    name: "auth".into()
                },
                &|_| None
            ),
            Err(reason)
        );
    }
    let resolver = SecretResolver::new(McpCredentialBackend::Unavailable(
        SecretUnavailableReason::Unavailable,
    ));
    let nonrecursive = resolver
        .resolve(
            &scope(),
            &ValueSource::Template {
                template: env("MISSING", Some("${NO_EXPANSION}")),
            },
            &|_| None,
        )
        .unwrap();
    assert_eq!(nonrecursive.expose(), "${NO_EXPANSION}");
    assert_eq!(
        resolver.resolve(
            &scope(),
            &ValueSource::Template {
                template: env("MISSING", None)
            },
            &|_| None
        ),
        Err(McpErrorKind::SecretUnavailable)
    );
}
#[test]
fn namespace_prevents_provider_and_other_project_access_and_allows_rotation() {
    let store = Arc::new(MemoryStore::default());
    let resolver = SecretResolver::new(McpCredentialBackend::Available(store.clone()));
    let provider = "anthropic.api_key";
    store
        .set(provider, &Secret::from("provider-secret".to_owned()))
        .unwrap();
    let reference = ValueSource::Keychain {
        name: provider.into(),
    };
    assert_eq!(
        resolver.resolve_classified(&scope(), &reference, &|_| None),
        Err(SecretUnavailableReason::Missing)
    );
    let key = credential_key(&scope(), provider);
    assert_ne!(key, provider);
    for value in ["old", "rotated"] {
        store.set(&key, &Secret::from(value.to_owned())).unwrap();
        assert_eq!(
            resolver
                .resolve(&scope(), &reference, &|_| None)
                .unwrap()
                .expose(),
            value
        );
    }
    let mut other = scope();
    other.canonical_root = "/other".into();
    assert_eq!(
        resolver.resolve_classified(&other, &reference, &|_| None),
        Err(SecretUnavailableReason::Missing)
    );
    other = scope();
    other.server_id = "other".into();
    assert_ne!(credential_key(&other, provider), key);
    // Fields are length framed, not concatenated with an ambiguous separator.
    other = scope();
    other.server_id = "server/a".into();
    assert_ne!(credential_key(&other, "b"), credential_key(&scope(), "a/b"));
}
struct Failure(SecretUnavailableReason);
impl CredentialStore for Failure {
    fn get(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
        Err(match self.0 {
            SecretUnavailableReason::Locked => CredentialError::KeychainLocked,
            SecretUnavailableReason::Denied => CredentialError::KeychainCancelled,
            _ => CredentialError::KeychainUnavailable {
                detail: "sentinel-native-secret".into(),
            },
        })
    }
    fn set(&self, _: &str, _: &Secret) -> Result<(), CredentialError> {
        unreachable!()
    }
    fn delete(&self, _: &str) -> Result<(), CredentialError> {
        unreachable!()
    }
}
#[test]
fn keychain_failure_never_falls_back_to_file() {
    // Injected failures never produce a value, even when an unrelated plaintext
    // credential store contains the exact MCP key. The resolver has no file path.
    let dir = tempdir().unwrap();
    let file = sandbox::FileCredentialStore::open(dir.path()).unwrap();
    file.set(
        &credential_key(&scope(), "auth"),
        &Secret::from("plaintext-trap".to_owned()),
    )
    .unwrap();
    for reason in [
        SecretUnavailableReason::Unavailable,
        SecretUnavailableReason::Locked,
        SecretUnavailableReason::Denied,
    ] {
        let resolver =
            SecretResolver::new(McpCredentialBackend::Available(Arc::new(Failure(reason))));
        assert_eq!(
            resolver.resolve_classified(
                &scope(),
                &ValueSource::Keychain {
                    name: "auth".into()
                },
                &|_| None
            ),
            Err(reason)
        );
        assert_eq!(
            resolver.resolve(
                &scope(),
                &ValueSource::Keychain {
                    name: "auth".into()
                },
                &|_| None
            ),
            Err(McpErrorKind::SecretUnavailable)
        );
    }
}
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[test]
fn secrets_absent_from_debug_events_errors_snapshots_and_ledger() {
    let logs = Capture::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    let sentinel = "sentinel-secret-must-not-leak";
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let source = ValueSource::Template {
        template: env("TOKEN", None),
    };
    let resolver = SecretResolver::new(McpCredentialBackend::Unavailable(
        SecretUnavailableReason::Unavailable,
    ));
    let value = resolver
        .resolve(&scope(), &source, &|_| Some(sentinel.into()))
        .unwrap();
    let def = McpServerDefinition {
        enabled: true,
        roles: vec!["worker".into()],
        startup_timeout_ms: 10000,
        request_timeout_ms: 60000,
        transport: McpTransportDefinition::Stdio {
            command: literal("server"),
            args: vec![literal(sentinel)],
            cwd: None,
            env: BTreeMap::from([("TOKEN".into(), source.clone())]),
        },
    };
    let current = inspect_stdio(
        scope(),
        SourceRevision::from_sources(None, None),
        &def,
        Path::new("/bin/server"),
        Path::new("/project"),
    )
    .unwrap();
    let preview = ledger.preview(&current).unwrap();
    let encoded = serde_json::to_string(&preview).unwrap();
    ledger
        .approve(
            &ConfirmMcpApproval {
                preview_id: preview.id,
                scope: preview.scope.clone(),
                expected_revision: preview.source_revision,
                expected_fingerprint: preview.fingerprint,
            },
            &current,
        )
        .unwrap();
    let permit = ledger.check(&current).unwrap();
    let resolved = ResolvedMcpServer {
        scope: scope(),
        fingerprint: current.fingerprint,
        transport: ResolvedMcpTransport::Stdio {
            executable: "/bin/server".into(),
            args: vec![value.clone()],
            cwd: "/project".into(),
            env: BTreeMap::from([("TOKEN".into(), value.clone())]),
        },
        startup_timeout: std::time::Duration::from_secs(10),
        request_timeout: std::time::Duration::from_secs(60),
    };
    let failing = SecretResolver::new(McpCredentialBackend::Available(Arc::new(Failure(
        SecretUnavailableReason::Unavailable,
    ))));
    let kind = failing
        .resolve(
            &scope(),
            &ValueSource::Keychain {
                name: "auth".into(),
            },
            &|_| None,
        )
        .unwrap_err();
    let error = McpError {
        server: "server".into(),
        method: "resolve",
        request_id: None,
        kind,
    };
    let status = McpStatusEvent {
        scope: scope(),
        status: McpServerStatus::SecretUnavailable,
        reason: Some(kind),
    };
    // Real snapshot construction/redaction is covered by runtime E/G; an empty
    // DTO here would not exercise that production boundary.
    let displays = [
        format!(
            "{value:?} {source:?} {def:?} {resolver:?} {preview:?} {permit:?} {resolved:?} {error:?} {error}"
        ),
        error.detail().to_string(),
        serde_json::to_string(&status).unwrap(),
        encoded,
        std::fs::read_to_string(dir.path().join("mcp-approvals.json")).unwrap(),
    ];
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(sentinel));
    assert!(!logs.contains("sentinel-native-secret"));
    for display in displays {
        assert!(!display.contains(sentinel));
        assert!(!display.contains("sentinel-native-secret"));
    }
}
