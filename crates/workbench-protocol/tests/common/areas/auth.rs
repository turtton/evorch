//! Samples of the operations the `auth` area adds. The secrets in
//! `save_api_key` and `submit_login_input` are obvious placeholders: a golden
//! file records the wire form of the input, which is the one place a secret
//! value legitimately appears.
use super::AreaSamples;
use workbench_protocol::commands::Command;
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::OperationId;
use workbench_protocol::namespaces::auth::*;
use workbench_protocol::namespaces::settings::{CredentialRef, ProviderKind};
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

fn operation(state: OperationState, kind: AuthOperationKind) -> AuthOperationView {
    AuthOperationView {
        operation: OperationView {
            operation_id: op("op-1"),
            state,
            revision: Revision(2),
            result: None,
        },
        kind,
        profile: Some("claude".into()),
        message: matches!(state, OperationState::Failed { .. })
            .then(|| "The login could not be completed.".into()),
    }
}

pub fn status() -> AuthStatus {
    AuthStatus {
        revision: Revision(7),
        slots: vec![
            CredentialSlot {
                profile: "claude".into(),
                kind: ProviderKind::AnthropicSubscription,
                storage: CredentialRef::Keyring {
                    service: "evorch".into(),
                    account: "claude".into(),
                },
                state: CredentialState::Present {
                    expires_at_unix: Some(4_102_444_800),
                },
                login: Some(LoginMethod::Claude),
            },
            CredentialSlot {
                profile: "work".into(),
                kind: ProviderKind::OpenaiCompatible,
                storage: CredentialRef::Env {
                    var: "WORK_API_KEY".into(),
                },
                state: CredentialState::Missing,
                login: None,
            },
            CredentialSlot {
                profile: "cursor".into(),
                kind: ProviderKind::Cursor,
                storage: CredentialRef::Keyring {
                    service: "evorch".into(),
                    account: "cursor".into(),
                },
                state: CredentialState::Unavailable,
                login: Some(LoginMethod::Cursor),
            },
        ],
    }
}

pub fn login() -> LoginView {
    LoginView {
        login_id: op("login-1"),
        profile: "claude".into(),
        method: LoginMethod::Claude,
        phase: LoginPhase::Waiting,
        authorize_url: Some("https://auth.example.test/authorize?state=abc".into()),
        accepts_input: true,
    }
}

pub fn samples() -> AreaSamples {
    AreaSamples {
        name: "auth",
        commands: vec![
            Command::Auth(AuthCommand::RefreshCredentials(RefreshCredentials {
                operation_id: op("op-refresh"),
            })),
            Command::Auth(AuthCommand::SaveApiKey(SaveApiKey {
                operation_id: op("op-key"),
                profile: "work".into(),
                value: SecretValue::new("placeholder-api-key"),
            })),
            Command::Auth(AuthCommand::DeleteCredential(DeleteCredential {
                operation_id: op("op-delete"),
                profile: "work".into(),
                account: "work-acct".into(),
                shared_with: vec!["work-2".into()],
            })),
            Command::Auth(AuthCommand::StartLogin(StartLogin {
                operation_id: op("login-1"),
                profile: "claude".into(),
            })),
            Command::Auth(AuthCommand::SubmitLoginInput(SubmitLoginInput {
                login_id: op("login-1"),
                input: SecretValue::new(
                    "http://localhost:54545/callback?code=placeholder&state=abc",
                ),
            })),
            Command::Auth(AuthCommand::CancelLogin(CancelLogin {
                login_id: op("login-1"),
            })),
        ],
        queries: vec![
            Query::Auth(AuthQuery::GetCredentialStatus {}),
            Query::Auth(AuthQuery::GetLogin {
                login_id: op("login-1"),
            }),
            Query::Auth(AuthQuery::GetAuthOperation {
                operation_id: op("op-1"),
            }),
            Query::Auth(AuthQuery::ListAuthOperations {}),
        ],
        ui_events: vec![
            UiEvent::Auth(AuthEvent::CredentialStatusChanged {
                revision: Revision(7),
            }),
            UiEvent::Auth(AuthEvent::LoginChanged {
                login_id: op("login-1"),
            }),
            UiEvent::Auth(AuthEvent::AuthOperationCompleted {
                operation: operation(
                    OperationState::Cancelled {
                        reason: CancelReason::Restart,
                    },
                    AuthOperationKind::Login,
                ),
            }),
        ],
        query_replies: vec![
            QueryReply::Auth(AuthReply::Status(status())),
            QueryReply::Auth(AuthReply::Login(login())),
            QueryReply::Auth(AuthReply::Operation(operation(
                OperationState::Pending,
                AuthOperationKind::Login,
            ))),
            QueryReply::Auth(AuthReply::Operations {
                operations: vec![
                    operation(OperationState::Ready, AuthOperationKind::SaveApiKey),
                    operation(
                        OperationState::Failed {
                            code: workbench_protocol::ErrorCode::StorageUnavailable,
                        },
                        AuthOperationKind::Login,
                    ),
                ],
            }),
        ],
    }
}
