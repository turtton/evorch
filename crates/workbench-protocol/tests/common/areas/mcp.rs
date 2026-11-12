//! Samples of the operations the `mcp` area adds. The secret in `save_secret`
//! is an obvious placeholder: a golden file records the wire form of the input,
//! which is the one place a secret value legitimately appears.
use super::AreaSamples;
use std::collections::BTreeMap;
use workbench_protocol::commands::Command;
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::ProjectId;
use workbench_protocol::namespaces::mcp::*;
use workbench_protocol::queries::{Query, QueryReply};

fn project() -> ProjectId {
    ProjectId::new("project-1").unwrap()
}

fn op(id: &str) -> McpOperationId {
    McpOperationId::new(id).unwrap()
}

fn scope() -> McpScope {
    McpScope {
        project: project(),
        server_id: "docs".into(),
    }
}

fn revision() -> McpRevision {
    McpRevision::from_bytes(&[0x11; 32])
}

fn fingerprint() -> McpFingerprint {
    McpFingerprint::from_bytes(&[0x22; 32])
}

fn literal(text: &str) -> McpTemplate {
    McpTemplate(vec![McpTemplatePart::Literal { value: text.into() }])
}

pub fn definition() -> McpServerDefinition {
    McpServerDefinition {
        enabled: true,
        roles: vec!["worker".into()],
        transport: McpTransport::Http {
            url: literal("https://mcp.example.test/rpc"),
            headers: BTreeMap::new(),
            token: Some(McpValueSource::Keychain {
                name: "docs-token".into(),
            }),
        },
        startup_timeout_ms: 10_000,
        request_timeout_ms: 60_000,
    }
}

pub fn stdio_definition() -> McpServerDefinition {
    McpServerDefinition {
        enabled: false,
        roles: vec![],
        transport: McpTransport::Stdio {
            command: literal("mcp-server"),
            args: vec![literal("--stdio")],
            cwd: Some(McpTemplate(vec![McpTemplatePart::Env {
                name: "HOME".into(),
                default: Some("/tmp".into()),
            }])),
            env: BTreeMap::from([(
                "LEVEL".into(),
                McpValueSource::Template {
                    template: literal("debug"),
                },
            )]),
        },
        startup_timeout_ms: 1,
        request_timeout_ms: 600_000,
    }
}

fn upsert() -> McpMutation {
    McpMutation::Upsert {
        server_id: "docs".into(),
        definition: definition(),
    }
}

fn summary() -> McpApprovalSummary {
    McpApprovalSummary {
        executable: None,
        canonical_cwd: None,
        destination: Some(McpDestination {
            scheme: "https".into(),
            host: "mcp.example.test".into(),
            port: 443,
        }),
        environment_keys: vec![],
        secret_references: vec!["docs-token".into()],
    }
}

fn preview() -> McpApprovalPreview {
    McpApprovalPreview {
        preview_id: McpPreviewId::from_bytes(&[0x33; 16]),
        scope: scope(),
        source_revision: revision(),
        fingerprint: fingerprint(),
        warnings: vec![McpApprovalWarning::SendsSecretReferences],
        previous_fingerprint: Some(McpFingerprint::from_bytes(&[0x44; 32])),
        previous_summary: Some(McpApprovalSummary::default()),
        summary: summary(),
        approved_ips: vec!["203.0.113.7".into()],
    }
}

fn operation(state: McpOperationState) -> McpOperationView {
    McpOperationView {
        operation_id: op("op-1"),
        project: project(),
        kind: McpOperationKind::TestConnection,
        server_id: Some("docs".into()),
        state,
    }
}

pub fn settings() -> McpSettings {
    McpSettings {
        project: project(),
        revision: revision(),
        preparation_blocked: false,
        diagnostics: vec![McpDiagnostic {
            server_id: None,
            source: McpOrigin::ProjectToml,
            code: McpConfigCode::Syntax,
        }],
        servers: vec![
            McpServerView {
                server_id: "docs".into(),
                origin: McpOrigin::ImportedJson,
                imported_exists: true,
                tombstone: false,
                definition: Some(definition()),
                diagnostics: vec![],
                state: McpServerState {
                    status: McpServerStatus::NeedsApproval,
                    reason: Some(McpErrorCode::NeedsApproval),
                },
                secret_references: vec!["docs-token".into()],
            },
            McpServerView {
                server_id: "local".into(),
                origin: McpOrigin::ProjectToml,
                imported_exists: false,
                tombstone: false,
                definition: Some(stdio_definition()),
                diagnostics: vec![],
                state: McpServerState {
                    status: McpServerStatus::Disabled,
                    reason: None,
                },
                secret_references: vec![],
            },
        ],
    }
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "mcp",
        commands: vec![
            Command::Mcp(McpCommand::Refresh(Refresh {
                operation_id: op("op-1"),
                project: project(),
            })),
            Command::Mcp(McpCommand::SaveOverride(SaveOverride {
                operation_id: op("op-2"),
                project: project(),
                expected_revision: revision(),
                mutation: upsert(),
            })),
            Command::Mcp(McpCommand::SetEnabled(SetEnabled {
                operation_id: op("op-3"),
                scope: scope(),
                expected_revision: revision(),
                enabled: false,
            })),
            Command::Mcp(McpCommand::PreviewApproval(PreviewApproval {
                operation_id: op("op-4"),
                scope: scope(),
                expected_revision: revision(),
            })),
            Command::Mcp(McpCommand::ConfirmApproval(ConfirmApproval {
                operation_id: op("op-5"),
                preview_id: McpPreviewId::from_bytes(&[0x33; 16]),
                scope: scope(),
                expected_revision: revision(),
                expected_fingerprint: fingerprint(),
            })),
            Command::Mcp(McpCommand::RevokeApproval(RevokeApproval {
                operation_id: op("op-6"),
                scope: scope(),
            })),
            Command::Mcp(McpCommand::TestConnection(TestConnection {
                operation_id: op("op-7"),
                scope: scope(),
                expected_revision: revision(),
            })),
            Command::Mcp(McpCommand::CancelOperation(CancelOperation {
                operation_id: op("op-7"),
            })),
            Command::Mcp(McpCommand::SaveSecret(SaveSecret {
                operation_id: op("op-8"),
                scope: scope(),
                logical_name: "docs-token".into(),
                value: McpSecretValue::new("placeholder-not-a-real-secret"),
                expected_revision: revision(),
                mutation: upsert(),
            })),
            Command::Mcp(McpCommand::DeleteSecret(DeleteSecret {
                operation_id: op("op-9"),
                scope: scope(),
                logical_name: "docs-token".into(),
                expected_revision: revision(),
                mutation: McpMutation::Upsert {
                    server_id: "docs".into(),
                    definition: stdio_definition(),
                },
            })),
        ],
        queries: vec![
            Query::Mcp(McpQuery::GetSettings { project: project() }),
            Query::Mcp(McpQuery::GetOperation {
                operation_id: op("op-1"),
            }),
            Query::Mcp(McpQuery::ListOperations {
                project: Some(project()),
            }),
        ],
        ui_events: vec![
            UiEvent::Mcp(McpEvent::SettingsChanged {
                project: project(),
                revision: revision(),
            }),
            UiEvent::Mcp(McpEvent::ServerStatusChanged {
                scope: scope(),
                state: McpServerState {
                    status: McpServerStatus::Unavailable,
                    reason: Some(McpErrorCode::HttpStatus { status: 503 }),
                },
            }),
            UiEvent::Mcp(McpEvent::OperationCompleted {
                operation: operation(McpOperationState::Partial {
                    outcome: McpOutcome::Settings {
                        revision: revision(),
                    },
                    keychain_error: McpErrorCode::SecretLocked,
                }),
            }),
        ],
        query_replies: vec![
            QueryReply::Mcp(McpReply::Settings(Box::new(settings()))),
            QueryReply::Mcp(McpReply::Operation(operation(McpOperationState::Running))),
            QueryReply::Mcp(McpReply::Operations {
                operations: vec![
                    operation(McpOperationState::Succeeded {
                        outcome: McpOutcome::ApprovalPreview {
                            preview: Box::new(preview()),
                        },
                    }),
                    operation(McpOperationState::Failed {
                        error: McpErrorCode::MissingEnvironment {
                            field: McpField::Url,
                        },
                    }),
                    operation(McpOperationState::Cancelled),
                    operation(McpOperationState::Succeeded {
                        outcome: McpOutcome::Tested {
                            status: McpServerStatus::Ready,
                        },
                    }),
                ],
            }),
        ],
    }
}
