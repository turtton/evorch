use config::mcp::{
    McpServerDefinition, McpTransportDefinition, SourceRevision, Template, TemplatePart,
    ValueSource,
};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, Barrier},
};
use tempfile::tempdir;
use tools::mcp::{
    McpErrorKind,
    approval::{ApprovalLedger, inspect_http, inspect_stdio},
    types::{ApprovalScope, ConfirmMcpApproval, McpApprovalInspection, McpApprovalStore},
};
use url::Url;
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
fn source(name: &str) -> ValueSource {
    ValueSource::Keychain { name: name.into() }
}
fn scope() -> ApprovalScope {
    ApprovalScope {
        canonical_root: "/project".into(),
        server_id: "server".into(),
    }
}
fn revision() -> SourceRevision {
    SourceRevision::from_sources(None, Some(b"original"))
}
fn definition() -> McpServerDefinition {
    McpServerDefinition {
        enabled: true,
        roles: vec!["worker".into()],
        startup_timeout_ms: 10000,
        request_timeout_ms: 60000,
        transport: McpTransportDefinition::Stdio {
            command: literal("server"),
            args: vec![env("ARG", Some("fallback"))],
            cwd: None,
            env: BTreeMap::from([
                ("TOKEN".into(), source("auth")),
                (
                    "REGION".into(),
                    ValueSource::Template {
                        template: literal("west"),
                    },
                ),
            ]),
        },
    }
}
fn inspect(def: &McpServerDefinition) -> McpApprovalInspection {
    inspect_stdio(
        scope(),
        revision(),
        def,
        Path::new("/bin/server"),
        Path::new("/project"),
    )
    .unwrap()
}
fn approve(ledger: &ApprovalLedger, inspection: &McpApprovalInspection) {
    let p = ledger.preview(inspection).unwrap();
    ledger
        .approve(
            &ConfirmMcpApproval {
                preview_id: p.id,
                scope: p.scope,
                expected_revision: p.source_revision,
                expected_fingerprint: p.fingerprint,
            },
            inspection,
        )
        .unwrap();
}
fn http(url: Template) -> McpServerDefinition {
    let mut def = definition();
    def.transport = McpTransportDefinition::Http {
        url,
        headers: BTreeMap::new(),
        token: None,
    };
    def
}
fn inspect_url(
    def: &McpServerDefinition,
    url: &str,
    ips: Vec<std::net::IpAddr>,
) -> McpApprovalInspection {
    inspect_http(scope(), revision(), def, &Url::parse(url).unwrap(), ips).unwrap()
}
#[test]
fn approval_projection_tracks_launch_and_secret_references() {
    let base = definition();
    let expected = inspect(&base).fingerprint;
    // Each meaningful launch/reference edit independently revokes consent.
    for kind in 0..7 {
        let mut changed = base.clone();
        let McpTransportDefinition::Stdio {
            command,
            args,
            env,
            cwd,
        } = &mut changed.transport
        else {
            unreachable!()
        };
        match kind {
            0 => *command = literal("other"),
            1 => args.push(literal("--write")),
            2 => args[0] = self::env("NEW_ARG", Some("fallback")),
            3 => args[0] = self::env("ARG", Some("changed")),
            4 => {
                env.insert("TOKEN".into(), source("new_auth"));
            }
            5 => {
                env.insert(
                    "REGION".into(),
                    ValueSource::Template {
                        template: literal("east"),
                    },
                );
            }
            6 => {
                env.insert("NEW".into(), source("auth"));
            }
            _ => unreachable!(),
        }
        *cwd = Some(literal("/project"));
        assert_ne!(expected, inspect(&changed).fingerprint, "change {kind}");
    }
    assert_ne!(
        expected,
        inspect_stdio(
            scope(),
            revision(),
            &base,
            Path::new("/new/bin/server"),
            Path::new("/project")
        )
        .unwrap()
        .fingerprint
    );
    assert_ne!(
        expected,
        inspect_stdio(
            scope(),
            revision(),
            &base,
            Path::new("/bin/server"),
            Path::new("/other")
        )
        .unwrap()
        .fingerprint
    );
    let def = http(literal("HTTPS://EXAMPLE.COM:443/a/../mcp?q=one"));
    let first = inspect_url(&def, "https://example.com/mcp?q=one", vec![]);
    assert_eq!(
        first.fingerprint,
        inspect_url(
            &http(literal("https://example.com/mcp?q=one")),
            "https://example.com/mcp?q=one",
            vec![]
        )
        .fingerprint
    );
    for url in [
        "http://example.com/mcp?q=one",
        "https://other.com/mcp?q=one",
        "https://example.com/other?q=one",
        "https://example.com/mcp?q=two",
    ] {
        assert_ne!(
            first.fingerprint,
            inspect_url(&http(literal(url)), url, vec![]).fingerprint
        );
    }
    let local = http(literal("http://localhost/mcp"));
    assert_ne!(
        inspect_url(
            &local,
            "http://localhost/mcp",
            vec!["127.0.0.1".parse().unwrap()]
        )
        .fingerprint,
        inspect_url(
            &local,
            "http://localhost/mcp",
            vec!["127.0.0.2".parse().unwrap()]
        )
        .fingerprint
    );
    let mut authenticated = def.clone();
    if let McpTransportDefinition::Http { token, .. } = &mut authenticated.transport {
        *token = Some(source("auth"));
    }
    assert_ne!(
        first.fingerprint,
        inspect_url(&authenticated, "https://example.com/mcp?q=one", vec![]).fingerprint
    );
}
#[test]
fn secret_rotation_inherited_env_and_roles_do_not_revoke_approval() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().join("user")).unwrap();
    let base = definition();
    let original = inspect(&base);
    approve(&ledger, &original);
    let mut changed = base.clone();
    changed.roles = vec!["reviewer".into()];
    changed.startup_timeout_ms = 1;
    changed.request_timeout_ms = 2;
    let mut current = inspect(&changed);
    current.source_revision = SourceRevision::from_sources(None, Some(b"roles changed"));
    assert_eq!(original.fingerprint, current.fingerprint);
    assert!(ledger.check(&current).is_ok());
    // Lookup is explicit and only the resolved executable, not inherited PATH's
    // text, enters inspect_stdio. Different PATH strings with the same outcome
    // and rotating reference values therefore keep the same inspection.
    let resolver = tools::mcp::secrets::SecretResolver::new(
        tools::mcp::types::McpCredentialBackend::Unavailable(
            tools::mcp::types::SecretUnavailableReason::Unavailable,
        ),
    );
    use tools::mcp::types::McpSecretResolver;
    for value in ["rotated-one", "rotated-two"] {
        let resolved = resolver
            .resolve(
                &scope(),
                &ValueSource::Template {
                    template: env("ARG", None),
                },
                &|_| Some(value.into()),
            )
            .unwrap();
        assert_eq!(resolved.expose(), value);
        assert_eq!(inspect(&base).fingerprint, original.fingerprint);
    }
    let template = Template(vec![
        TemplatePart::Literal {
            value: "https://example.com/mcp?key=".into(),
        },
        TemplatePart::Env {
            name: "TOKEN".into(),
            default: None,
        },
    ]);
    let def = http(template);
    assert_eq!(
        inspect_url(&def, "https://example.com/mcp?key=one", vec![]).fingerprint,
        inspect_url(&def, "https://example.com/mcp?key=two", vec![]).fingerprint
    );
    let endpoint = http(env("ENDPOINT", None));
    assert_eq!(
        inspect_url(&endpoint, "https://example.com/a", vec![]).fingerprint,
        inspect_url(&endpoint, "https://example.com/b", vec![]).fingerprint
    );
    assert_ne!(
        inspect_url(&endpoint, "https://example.com/a", vec![]).fingerprint,
        inspect_url(&endpoint, "https://other.com/a", vec![]).fingerprint
    );
}
#[test]
fn approval_preview_rejects_stale_revision() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let initial = inspect(&definition());
    for change in 0..4 {
        let p = ledger.preview(&initial).unwrap();
        let mut current = initial.clone();
        let mut confirmation = ConfirmMcpApproval {
            preview_id: p.id,
            scope: p.scope,
            expected_revision: p.source_revision,
            expected_fingerprint: p.fingerprint,
        };
        match change {
            0 => current.source_revision = SourceRevision::from_sources(None, Some(b"edit")),
            1 => current.fingerprint = tools::mcp::types::ApprovalFingerprint([9; 32]),
            2 => {
                confirmation.expected_revision =
                    SourceRevision::from_sources(None, Some(b"stale UI"))
            }
            3 => confirmation.preview_id = tools::mcp::types::ApprovalPreviewId([0; 16]),
            _ => unreachable!(),
        }
        assert_eq!(
            ledger.approve(&confirmation, &current),
            Err(McpErrorKind::StalePreview)
        );
        assert!(matches!(
            ledger.check(&initial),
            Err(McpErrorKind::NeedsApproval)
        ));
    }
    let p = ledger.preview(&initial).unwrap();
    ledger.revoke(&initial.scope).unwrap();
    assert_eq!(
        ledger.approve(
            &ConfirmMcpApproval {
                preview_id: p.id,
                scope: p.scope,
                expected_revision: p.source_revision,
                expected_fingerprint: p.fingerprint
            },
            &initial
        ),
        Err(McpErrorKind::StalePreview)
    );
}
#[test]
fn approving_the_same_fingerprint_preserves_live_permits() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let permit = ledger.check(&current).unwrap();
    let other = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let other_permit = other.check(&current).unwrap();
    let mut edited = current.clone();
    edited.source_revision = SourceRevision::from_sources(None, Some(b"roles changed"));
    approve(&other, &edited);
    assert!(!permit.is_revoked());
    assert!(!other_permit.is_revoked());
    assert!(!ledger.check(&edited).unwrap().is_revoked());
}

#[test]
fn approving_a_different_fingerprint_still_revokes_old_permits() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let permit = ledger.check(&current).unwrap();
    let mut edited = current.clone();
    edited.fingerprint = tools::mcp::types::ApprovalFingerprint::from_projection(b"new launch");
    approve(&ledger, &edited);
    assert!(permit.is_revoked());
    assert!(!ledger.check(&edited).unwrap().is_revoked());
    // Returning to the original grant does not resurrect the old session.
    approve(&ledger, &current);
    assert!(permit.is_revoked());
}

#[test]
fn revocation_invalidates_live_permit() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let permit = ledger.check(&current).unwrap();
    let other = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    assert!(!permit.is_revoked());
    other.revoke(&current.scope).unwrap();
    assert!(permit.is_revoked());
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    approve(&ledger, &current);
    assert!(ledger.check(&current).is_ok());
    assert!(permit.is_revoked());
}
#[test]
fn corrupt_ledger_is_unapproved() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let permit = ledger.check(&current).unwrap();
    std::fs::write(dir.path().join("mcp-approvals.json"), b"{broken").unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    assert!(permit.is_revoked());
    // Corruption can only be replaced by another explicit, current preview.
    approve(&ledger, &current);
    assert!(ledger.check(&current).is_ok());
}
#[test]
fn public_https_without_references_needs_no_confirmation_but_revoke_persists() {
    let dir = tempdir().unwrap();
    let current = inspect_url(
        &http(literal("https://example.com/mcp")),
        "https://example.com/mcp",
        vec![],
    );
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let permit = ledger.check(&current).unwrap();
    ledger.revoke(&current.scope).unwrap();
    assert!(permit.is_revoked());
    drop(ledger);
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    approve(&ledger, &current);
    assert!(ledger.check(&current).is_ok());
}
#[test]
fn ledger_permissions_atomic_updates_and_scope_isolation() {
    let dir = tempdir().unwrap();
    let root = dir.path().join("user");
    let ledger = ApprovalLedger::at_user_config_dir(root.clone()).unwrap();
    assert!(!root.exists());
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|i| {
            let ledger = ledger.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut current = inspect(&definition());
                current.scope.server_id = format!("server{i}");
                barrier.wait();
                approve(&ledger, &current);
                current
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        assert!(ledger.check(&worker.join().unwrap()).is_ok());
    }
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(root.join("mcp-approvals.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(matches!(
        ledger.check(&inspect(&definition())),
        Err(McpErrorKind::NeedsApproval)
    ));
}
#[test]
fn ledger_symlinks_do_not_authorize() {
    let dir = tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(link).unwrap();
    assert!(ledger.preview(&inspect(&definition())).is_err());
    let ledger = ApprovalLedger::at_user_config_dir(real.clone()).unwrap();
    approve(&ledger, &inspect(&definition()));
    let backup = dir.path().join("backup");
    std::fs::rename(real.join("mcp-approvals.json"), &backup).unwrap();
    std::os::unix::fs::symlink(&backup, real.join("mcp-approvals.json")).unwrap();
    assert!(matches!(
        ledger.check(&inspect(&definition())),
        Err(McpErrorKind::NeedsApproval)
    ));
}

#[test]
fn failed_revocation_write_cannot_reauthorize_live_sessions() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("user");
    let backup = dir.path().join("backup");
    let ledger = ApprovalLedger::at_user_config_dir(path.clone()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let permit = ledger.check(&current).unwrap();
    std::fs::rename(&path, &backup).unwrap();
    std::fs::write(&path, b"not a directory").unwrap();
    assert!(ledger.revoke(&current.scope).is_err());
    assert!(permit.is_revoked());
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(backup, path).unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    approve(&ledger, &current);
    assert!(ledger.check(&current).is_ok());
}
#[test]
fn projection_tracks_reference_defaults_and_literal_reference_switches() {
    let base = definition();
    let mut changed = base.clone();
    if let McpTransportDefinition::Stdio { env, .. } = &mut changed.transport {
        env.insert(
            "TOKEN".into(),
            ValueSource::Template {
                template: literal("literal-token"),
            },
        );
    }
    assert_ne!(inspect(&base).fingerprint, inspect(&changed).fingerprint);
    if let McpTransportDefinition::Stdio { cwd, .. } = &mut changed.transport {
        *cwd = Some(env("ROOT", Some("/project")));
    }
    let cwd_ref = inspect(&changed).fingerprint;
    if let McpTransportDefinition::Stdio { cwd, .. } = &mut changed.transport {
        *cwd = Some(env("ROOT", Some("/different-default")));
    }
    assert_ne!(cwd_ref, inspect(&changed).fingerprint);
    let mut def = http(literal("https://example.com/mcp"));
    if let McpTransportDefinition::Http { headers, .. } = &mut def.transport {
        headers.insert(
            "Authorization".into(),
            ValueSource::Template {
                template: env("TOKEN", Some("default")),
            },
        );
    }
    let reference = inspect_url(&def, "https://example.com/mcp", vec![]).fingerprint;
    for source in [
        ValueSource::Template {
            template: env("OTHER", Some("default")),
        },
        ValueSource::Template {
            template: env("TOKEN", Some("changed")),
        },
        source("auth"),
        ValueSource::Template {
            template: literal("literal"),
        },
    ] {
        let mut changed = def.clone();
        if let McpTransportDefinition::Http { headers, .. } = &mut changed.transport {
            headers.insert("Authorization".into(), source);
        }
        assert_ne!(
            reference,
            inspect_url(&changed, "https://example.com/mcp", vec![]).fingerprint
        );
    }
}

#[test]
fn ledger_nonregular_files_and_insecure_permissions_fail_closed() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    approve(&ledger, &current);
    let file = dir.path().join("mcp-approvals.json");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    std::fs::remove_file(&file).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &file,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
}

#[test]
fn revocation_finds_permits_that_outlive_store_handles() {
    let dir = tempdir().unwrap();
    let current = inspect(&definition());
    let permit = {
        let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
        approve(&ledger, &current);
        ledger.check(&current).unwrap()
    };
    let reopened = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    reopened.revoke(&current.scope).unwrap();
    assert!(permit.is_revoked());
}

#[test]
fn unapproved_execution_check_does_not_consume_pending_gui_preview() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let current = inspect(&definition());
    let preview = ledger.preview(&current).unwrap();
    assert!(matches!(
        ledger.check(&current),
        Err(McpErrorKind::NeedsApproval)
    ));
    ledger
        .approve(
            &ConfirmMcpApproval {
                preview_id: preview.id,
                scope: preview.scope,
                expected_revision: preview.source_revision,
                expected_fingerprint: preview.fingerprint,
            },
            &current,
        )
        .unwrap();
    assert!(ledger.check(&current).is_ok());
}

#[test]
fn production_ledger_uses_user_config_dir_without_project_fallback() {
    const CHILD: &str = "EVORCH_MCP_B_USER_DIR_TEST";
    if let Ok(mode) = std::env::var(CHILD) {
        let current = inspect(&definition());
        match mode.as_str() {
            "available" => {
                let user_dir = config::user_config_dir().unwrap();
                let ledger = ApprovalLedger::open_user().unwrap();
                approve(&ledger, &current);
                assert!(user_dir.join("mcp-approvals.json").is_file());
                assert!(
                    !std::env::current_dir()
                        .unwrap()
                        .join("mcp-approvals.json")
                        .exists()
                );
            }
            "missing" => {
                assert!(config::user_config_dir().is_none());
                assert!(matches!(
                    ApprovalLedger::open_user(),
                    Err(McpErrorKind::Unavailable)
                ));
                assert_eq!(
                    std::fs::read_dir(std::env::current_dir().unwrap())
                        .unwrap()
                        .count(),
                    0
                );
            }
            _ => panic!("unexpected test mode"),
        }
        return;
    }
    // Child environments isolate XDG/HOME without process-global set_var races.
    let dir = tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    for mode in ["available", "missing"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "production_ledger_uses_user_config_dir_without_project_fallback",
                "--nocapture",
            ])
            .env(CHILD, mode)
            .env_remove("HOME")
            .env_remove("XDG_CONFIG_HOME")
            .current_dir(&project);
        if mode == "available" {
            child.env("XDG_CONFIG_HOME", dir.path().join("xdg"));
        }
        let result = child.output().unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn non_utf8_launch_paths_are_classified_without_panicking() {
    use std::os::unix::ffi::OsStringExt;
    let invalid =
        std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/bin/secret-\xff".to_vec()));
    assert!(matches!(
        inspect_stdio(
            scope(),
            revision(),
            &definition(),
            &invalid,
            Path::new("/project")
        ),
        Err(McpErrorKind::Configuration)
    ));
    assert!(matches!(
        inspect_stdio(
            scope(),
            revision(),
            &definition(),
            Path::new("/bin/server"),
            &invalid
        ),
        Err(McpErrorKind::Configuration)
    ));
}

#[test]
fn removing_secret_references_does_not_force_unneeded_public_https_confirmation() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let mut authenticated = http(literal("https://example.com/mcp"));
    if let McpTransportDefinition::Http { token, .. } = &mut authenticated.transport {
        *token = Some(source("auth"));
    }
    approve(
        &ledger,
        &inspect_url(&authenticated, "https://example.com/mcp", vec![]),
    );
    let public = inspect_url(
        &http(literal("https://other.example/mcp")),
        "https://other.example/mcp",
        vec![],
    );
    assert!(ledger.check(&public).is_ok());
    ledger.revoke(&public.scope).unwrap();
    assert!(matches!(
        ledger.check(&public),
        Err(McpErrorKind::NeedsApproval)
    ));
}

#[test]
fn previous_approval_summary_is_the_recorded_safe_metadata_not_current_values() {
    let dir = tempdir().unwrap();
    let ledger = ApprovalLedger::at_user_config_dir(dir.path().into()).unwrap();
    let mut first = definition();
    let McpTransportDefinition::Stdio { env, args, .. } = &mut first.transport else {
        panic!("stdio")
    };
    env.insert(
        "API_TOKEN".into(),
        ValueSource::Template {
            template: literal("env-secret-sentinel"),
        },
    );
    args.push(literal("--token=arg-secret-sentinel"));
    let before = inspect(&first);
    approve(&ledger, &before);
    let raw = std::fs::read_to_string(dir.path().join("mcp-approvals.json")).unwrap();
    assert!(!raw.contains("env-secret-sentinel"));
    assert!(!raw.contains("arg-secret-sentinel"));
    let mut second = definition();
    let McpTransportDefinition::Stdio { env, .. } = &mut second.transport else {
        panic!("stdio")
    };
    env.insert("NEW_KEY".into(), source("new-ref"));
    let current = inspect(&second);
    let preview = ledger.preview(&current).unwrap();
    assert_eq!(preview.previous_summary.as_ref(), Some(&before.summary));
    assert_eq!(preview.summary, current.summary);
    assert_ne!(preview.summary, before.summary);
    assert!(!format!("{preview:?}").contains("secret-sentinel"));
}
