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
            kind: Some(ModelKind::Chat),
            context_override: Some(200_000),
        },
        ModelInput {
            id: "model-b".into(),
            enabled: false,
            kind: None,
            context_override: None,
        },
    ]
}

pub fn snapshot() -> SettingsSnapshot {
    SettingsSnapshot {
        revision: revision(0x22),
        applied_revision: Some(revision(0x11)),
        apply_failure: None,
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
                kind: ModelKind::Chat,
                context_override: Some(200_000),
            }],
            excluded_models: vec!["model-z".into()],
            default_model: "model-a".into(),
            show_quota: false,
            quota_supported: true,
        }],
        problem: None,
    }
}

fn pull_requests() -> PullRequestFeature {
    PullRequestFeature {
        enabled: true,
        auto_fix_enabled: true,
        include_in_review: false,
        pending_poll_seconds: 30,
        stable_poll_seconds: 300,
        timeout_seconds: 20,
    }
}

fn browser() -> BrowserFeature {
    BrowserFeature {
        enabled: true,
        executable: Some("/usr/bin/chromium".into()),
        max_pages: 8,
        max_tabs: 4,
        coordinate_actions: false,
    }
}

fn feature_values(browser_enabled: bool) -> FeatureValues {
    FeatureValues {
        pull_requests: pull_requests(),
        agent_web_tools: true,
        browser: BrowserFeature {
            enabled: browser_enabled,
            ..browser()
        },
        thread_cleanup: ThreadCleanupFeature {
            auto_archive_after_days: 7,
            archived_delete_after_days: 7,
        },
    }
}

pub fn feature_settings() -> FeatureSettingsView {
    FeatureSettingsView {
        revision: revision(0x22),
        saved: feature_values(false),
        running: feature_values(true),
        restart_pending: vec![FeatureSection::Browser],
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
                    show_quota: Some(false),
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
            Command::Settings(SettingsCommand::SaveFeatureSettings(SaveFeatureSettings {
                operation_id: op("op-features"),
                expected_revision: revision(0x11),
                changes: FeatureSettingsPatch {
                    pull_requests: Some(pull_requests()),
                    agent_web_tools: Some(false),
                    browser: Some(browser()),
                    thread_cleanup: Some(ThreadCleanupFeature {
                        auto_archive_after_days: 0,
                        archived_delete_after_days: 30,
                    }),
                },
            })),
            Command::Settings(SettingsCommand::FetchProviderModels(FetchProviderModels {
                operation_id: op("op-fetch"),
                expected_revision: revision(0x11),
                provider: "work".into(),
            })),
        ],
        queries: vec![
            Query::Settings(SettingsQuery::ModelChoices {
                project: workbench_protocol::ids::ProjectId::new("project-1").unwrap(),
            }),
            Query::Settings(SettingsQuery::GetSettingsSnapshot {}),
            Query::Settings(SettingsQuery::GetFeatureSettings {}),
            Query::Settings(SettingsQuery::GetSettingsOperation {
                operation_id: op("op-1"),
            }),
            Query::Settings(SettingsQuery::ListSettingsOperations {}),
            Query::Settings(SettingsQuery::GetProviderModels {
                operation_id: op("op-fetch"),
            }),
        ],
        ui_events: vec![
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged {
                revision: revision(0x22),
                applied_revision: Some(revision(0x11)),
            }),
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted {
                operation: operation(OperationState::Ready, SettingsOperationKind::SaveDocument),
            }),
        ],
        query_replies: vec![
            QueryReply::Settings(SettingsReply::ModelChoices(ProjectModelChoices {
                project: workbench_protocol::ids::ProjectId::new("project-1").unwrap(),
                profiles: vec![],
                worker: RoleModelChoice {
                    model: "work/model-a".into(),
                    reasoning_effort: Some("high".into()),
                },
                orchestrator: RoleModelChoice {
                    model: "work/model-b".into(),
                    reasoning_effort: None,
                },
            })),
            QueryReply::Settings(SettingsReply::Settings(Box::new(snapshot()))),
            QueryReply::Settings(SettingsReply::FeatureSettings(Box::new(feature_settings()))),
            QueryReply::Settings(SettingsReply::Operation(operation(
                OperationState::Pending,
                SettingsOperationKind::ReloadModels,
            ))),
            QueryReply::Settings(SettingsReply::ProviderModels(ProviderModelCandidates {
                operation_id: op("op-fetch"),
                provider: "work".into(),
                models: vec!["model-a".into(), "model-b".into()],
                truncated: true,
            })),
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
