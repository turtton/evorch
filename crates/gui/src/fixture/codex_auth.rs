//! 外部認証を行わず結果と完了タイミングを制御する fixture。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use crate::model::codex_auth::{
    CODEX_DEVICE_URL, CodexAuthBackend, CodexAuthError, CodexAuthSummary, CodexUserCodePrompt,
};

enum ScriptedOutcome {
    Gated(Receiver<Result<CodexAuthSummary, CodexAuthError>>),
    Immediate(Result<CodexAuthSummary, CodexAuthError>),
}

pub struct ScriptedCodexAuthBackend {
    stored: Mutex<Result<Option<CodexAuthSummary>, CodexAuthError>>,
    outcome: Mutex<ScriptedOutcome>,
    calls: AtomicUsize,
}

impl ScriptedCodexAuthBackend {
    pub fn prompt() -> CodexUserCodePrompt {
        CodexUserCodePrompt {
            user_code: "ABCD-1234".into(),
            verification_url: CODEX_DEVICE_URL.into(),
        }
    }

    pub fn gated() -> (Arc<Self>, Sender<Result<CodexAuthSummary, CodexAuthError>>) {
        let (tx, rx) = channel();
        (
            Arc::new(Self {
                stored: Mutex::new(Ok(None)),
                outcome: Mutex::new(ScriptedOutcome::Gated(rx)),
                calls: AtomicUsize::new(0),
            }),
            tx,
        )
    }

    pub fn immediate(outcome: Result<CodexAuthSummary, CodexAuthError>) -> Arc<Self> {
        Arc::new(Self {
            stored: Mutex::new(Ok(None)),
            outcome: Mutex::new(ScriptedOutcome::Immediate(outcome)),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn authenticated(summary: CodexAuthSummary) -> Arc<Self> {
        Arc::new(Self {
            stored: Mutex::new(Ok(Some(summary.clone()))),
            outcome: Mutex::new(ScriptedOutcome::Immediate(Ok(summary))),
            calls: AtomicUsize::new(0),
        })
    }

    pub fn set_stored(&self, stored: Result<Option<CodexAuthSummary>, CodexAuthError>) {
        *self.stored.lock().unwrap_or_else(PoisonError::into_inner) = stored;
    }

    pub fn authenticate_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl CodexAuthBackend for ScriptedCodexAuthBackend {
    fn load_summary(&self) -> Result<Option<CodexAuthSummary>, CodexAuthError> {
        self.stored
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn authenticate(
        &self,
        on_prompt: &mut (dyn FnMut(CodexUserCodePrompt) + Send),
    ) -> Result<CodexAuthSummary, CodexAuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        on_prompt(Self::prompt());
        match &*self.outcome.lock().unwrap_or_else(PoisonError::into_inner) {
            ScriptedOutcome::Gated(rx) => rx.recv().map_err(|_| CodexAuthError::Unavailable)?,
            ScriptedOutcome::Immediate(outcome) => outcome.clone(),
        }
    }
}
