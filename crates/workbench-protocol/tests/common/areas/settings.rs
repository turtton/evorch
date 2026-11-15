//! Samples of the operations the `settings` area adds.
use super::AreaSamples;
use workbench_protocol::commands::Command;
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::OperationId;
use workbench_protocol::namespaces::settings::*;
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

fn revision(byte: u8) -> SettingsRevision {
    SettingsRevision::from_bytes(&[byte; 32])
}

fn operation(state: OperationState, kind: SettingsOperationKind) -> SettingsOperationView {
    SettingsOperationView {
        operation: OperationView {
            operation_id: op("op-1"),
            state,
            revision: Revision(4),
            result: None,
        },
        kind,
        revision: matches!(state, OperationState::Ready).then(|| revision(0x22)),
        message: matches!(state, OperationState::Failed { .. })
            .then(|| "core.restart.targets: restart targets may only be changed locally".into()),
    }
}

fn models() -> Vec<ModelInput> {
    vec![
        ModelInput {
            id: "model-a".into(),
            enabled: true,
        },
        ModelInput {
            id: "model-b".into(),
            enabled: false,
        },
    ]
}

pub fn snapshot() -> SettingsSnapshot {
    SettingsSnapshot {
        revision: revision(0x22),
        applied_revision: revision(0x11),
        document: "version = 3\n[core]\nidle_grace_secs = 30\n".into(),
        providers: vec![ProviderView {
            name: "work".into(),
            kind: ProviderKind::OpenaiCompatible,
            base_url: "https://api.example.test/v1".into(),
            credential: CredentialRef::Keyring {
                service: "evorch".into(),
                account: "work".into(),
            },
            models: vec![ModelView {
                id: "model-a".into(),
                enabled: true,
            }],
            excluded_models: vec!["model-z".into()],
            default_model: "model-a".into(),
        }],
        problem: None,
    }
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "settings",
        commands: vec![
            Command::Settings(SettingsCommand::RefreshSettings(RefreshSettings {
                operation_id: op("op-refresh"),
            })),
            Command::Settings(SettingsCommand::SaveDocument(SaveDocument {
                operation_id: op("op-save"),
                expected_revision: revision(0x11),
                text: "version = 3\n".into(),
            })),
            Command::Settings(SettingsCommand::SaveProvider(Box::new(SaveProvider {
                operation_id: op("op-provider-oa"),
                expected_revision: revision(0x11),
                original_name: Some("old".into()),
                provider: ProviderInput::OpenaiCompatible {
                    name: "work".into(),
                    kind: OpenaiCompatibleKind::OpenaiCompatible,
                    base_url: "https://api.example.test/v1".into(),
                    credential: CredentialRef::Env {
                        var: "WORK_API_KEY".into(),
                    },
                    models: models(),
                    excluded_models: vec![],
                    default_model: "model-a".into(),
                },
            }))),
            Command::Settings(SettingsCommand::DeleteProvider(DeleteProvider {
                operation_id: op("op-delete"),
                expected_revision: revision(0x11),
                name: "work".into(),
            })),
            Command::Settings(SettingsCommand::ReloadModels(ReloadModels {
                operation_id: op("op-reload"),
            })),
        ],
        queries: vec![
            Query::Settings(SettingsQuery::GetSettingsSnapshot {}),
            Query::Settings(SettingsQuery::GetSettingsOperation {
                operation_id: op("op-1"),
            }),
            Query::Settings(SettingsQuery::ListSettingsOperations {}),
        ],
        ui_events: vec![
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged {
                revision: revision(0x22),
                applied_revision: revision(0x11),
            }),
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted {
                operation: operation(OperationState::Ready, SettingsOperationKind::SaveDocument),
            }),
        ],
        query_replies: vec![
            QueryReply::Settings(SettingsReply::Settings(Box::new(snapshot()))),
            QueryReply::Settings(SettingsReply::Operation(operation(
                OperationState::Pending,
                SettingsOperationKind::ReloadModels,
            ))),
            QueryReply::Settings(SettingsReply::Operations {
                operations: vec![
                    operation(
                        OperationState::Failed {
                            code: workbench_protocol::ErrorCode::InvalidRequest,
                        },
                        SettingsOperationKind::SaveDocument,
                    ),
                    operation(
                        OperationState::Cancelled {
                            reason: CancelReason::Shutdown,
                        },
                        SettingsOperationKind::Refresh,
                    ),
                ],
            }),
        ],
    }
}
