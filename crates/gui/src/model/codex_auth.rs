//! egui に依存しない Codex browser 認証状態。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

pub const CODEX_LOGIN_BUTTON: &str = "Sign in with browser";
pub const CODEX_UNAUTHENTICATED_GUIDANCE: &str = "Sign in to Codex in your browser. Credentials are saved directly to the credential store; nothing is typed in the GUI.";
pub const CODEX_REQUESTING_LABEL: &str = "Preparing browser sign-in…";
pub const CODEX_WAITING_LABEL: &str = "Waiting for browser sign-in…";
pub const CODEX_AUTHENTICATED_LABEL: &str = "Authenticated";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CodexAuthError {
    #[error("Codex network unavailable")]
    Network,
    #[error("Codex credential store unavailable")]
    StoreUnavailable,
    #[error("Codex authorization rejected")]
    Rejected,
    #[error("Codex login unavailable")]
    Unavailable,
    #[error("Codex callback ports 1455 and 1457 are busy")]
    CallbackPortBusy,
    #[error("Codex browser sign-in timed out")]
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLoginPrompt {
    pub authorize_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexAuthSummary {
    pub expires_at_unix: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAuthState {
    Unauthenticated,
    Authenticating {
        prompt: Option<CodexLoginPrompt>,
        opened_browser: bool,
    },
    Authenticated {
        expires_at_unix: Option<u64>,
    },
    Failed {
        failure: CodexAuthError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexAuthEvent {
    Prompt(CodexLoginPrompt),
    Finished(Result<CodexAuthSummary, CodexAuthError>),
}

/// トークン本体を GUI に渡さない認証境界。
pub trait CodexAuthBackend: Send + Sync {
    /// 保存済み認証情報の要約を読み取る。
    ///
    /// # Errors
    /// ストアの読み取り失敗を返す。
    fn load_summary(&self) -> Result<Option<CodexAuthSummary>, CodexAuthError>;

    /// コードを通知して認証を完了し、保存済み情報の要約を返す。
    ///
    /// # Errors
    /// 認証または資格情報の保存失敗を返す。
    fn authenticate(
        &self,
        on_prompt: &mut (dyn FnMut(CodexLoginPrompt) + Send),
    ) -> Result<CodexAuthSummary, CodexAuthError>;
}

pub struct CodexAuthModel {
    pub state: CodexAuthState,
    pub credential_account: String,
    backend: Option<Arc<dyn CodexAuthBackend>>,
    rx: Option<Receiver<CodexAuthEvent>>,
}

impl Default for CodexAuthModel {
    fn default() -> Self {
        Self {
            state: CodexAuthState::Unauthenticated,
            credential_account: "codex".into(),
            backend: None,
            rx: None,
        }
    }
}

impl std::fmt::Debug for CodexAuthModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            CodexAuthState::Unauthenticated => "Unauthenticated",
            CodexAuthState::Authenticating { .. } => "Authenticating",
            CodexAuthState::Authenticated { .. } => "Authenticated",
            CodexAuthState::Failed { .. } => "Failed",
        };
        f.debug_struct("CodexAuthModel")
            .field("state", &state)
            .field("credential_account", &self.credential_account)
            .field("backend", &self.backend.as_ref().map(|_| "..."))
            .field("rx", &self.rx.as_ref().map(|_| "..."))
            .finish()
    }
}

impl CodexAuthModel {
    pub fn with_backend(
        backend: Arc<dyn CodexAuthBackend>,
        credential_account: impl Into<String>,
    ) -> Self {
        let mut model = Self {
            backend: Some(backend),
            credential_account: credential_account.into(),
            ..Self::default()
        };
        model.refresh_from_store();
        model
    }

    pub const fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    pub const fn is_authenticating(&self) -> bool {
        matches!(self.state, CodexAuthState::Authenticating { .. })
    }

    pub fn refresh_from_store(&mut self) {
        if self.is_authenticating() {
            return;
        }
        self.state = match &self.backend {
            None => CodexAuthState::Unauthenticated,
            Some(backend) => match backend.load_summary() {
                Ok(Some(summary)) => CodexAuthState::Authenticated {
                    expires_at_unix: summary.expires_at_unix,
                },
                Ok(None) => CodexAuthState::Unauthenticated,
                Err(failure) => CodexAuthState::Failed { failure },
            },
        };
    }

    pub fn start(&mut self) {
        if self.rx.is_some() || self.is_authenticating() {
            return;
        }
        let Some(backend) = self.backend.clone() else {
            self.state = CodexAuthState::Failed {
                failure: CodexAuthError::Unavailable,
            };
            return;
        };
        self.state = CodexAuthState::Authenticating {
            prompt: None,
            opened_browser: false,
        };
        let (tx, rx) = channel();
        match std::thread::Builder::new()
            .name("evorch-codex-login".into())
            .spawn(move || {
                let result = backend.authenticate(&mut |prompt| {
                    let _ = tx.send(CodexAuthEvent::Prompt(prompt));
                });
                let _ = tx.send(CodexAuthEvent::Finished(result));
            }) {
            Ok(_) => self.rx = Some(rx),
            Err(_) => {
                self.state = CodexAuthState::Failed {
                    failure: CodexAuthError::Unavailable,
                }
            }
        }
    }

    /// 未処理イベントを反映し、状態が変わった場合だけ true を返す。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.take() else {
            return false;
        };
        let mut changed = false;
        loop {
            let next = match rx.try_recv() {
                Ok(CodexAuthEvent::Prompt(prompt)) => {
                    let next = CodexAuthState::Authenticating {
                        prompt: Some(prompt),
                        opened_browser: false,
                    };
                    changed |= self.state != next;
                    self.state = next;
                    continue;
                }
                Ok(CodexAuthEvent::Finished(Ok(summary))) => CodexAuthState::Authenticated {
                    expires_at_unix: summary.expires_at_unix,
                },
                Ok(CodexAuthEvent::Finished(Err(failure))) => CodexAuthState::Failed { failure },
                Err(TryRecvError::Empty) => {
                    self.rx = Some(rx);
                    break;
                }
                Err(TryRecvError::Disconnected) => CodexAuthState::Failed {
                    failure: CodexAuthError::Unavailable,
                },
            };
            changed |= self.state != next;
            self.state = next;
            break;
        }
        changed
    }

    pub fn take_url_to_open(&mut self) -> Option<String> {
        match &mut self.state {
            CodexAuthState::Authenticating {
                prompt: Some(prompt),
                opened_browser,
            } if !*opened_browser => {
                *opened_browser = true;
                Some(prompt.authorize_url.clone())
            }
            CodexAuthState::Authenticating { .. }
            | CodexAuthState::Unauthenticated
            | CodexAuthState::Authenticated { .. }
            | CodexAuthState::Failed { .. } => None,
        }
    }

    #[cfg(test)]
    fn set_receiver_for_test(&mut self, rx: Receiver<CodexAuthEvent>) {
        self.rx = Some(rx);
    }
}

pub fn format_expiry(now_unix: u64, expires_at_unix: u64) -> String {
    if expires_at_unix <= now_unix {
        return "Access token expired; it is refreshed automatically on the next request".into();
    }
    let minutes = (expires_at_unix - now_unix) / 60;
    let hours = minutes / 60;
    if hours > 0 {
        format!("Access token expires in {hours}h {}m", minutes % 60)
    } else {
        format!("Access token expires in {minutes}m")
    }
}

#[cfg(test)]
#[path = "codex_auth_tests.rs"]
mod tests;
