use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::fixture::ScriptedCodexAuthBackend;

fn poll_until(model: &mut CodexAuthModel, pred: impl Fn(&CodexAuthState) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        model.poll();
        if pred(&model.state) {
            return;
        }
        assert!(Instant::now() < deadline, "state: {:?}", model.state);
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn default_model_is_unauthenticated_without_backend() {
    // Given / When: backend なしの初期モデル。
    let model = CodexAuthModel::default();
    // Then: 未認証で既定アカウントを使う。
    assert_eq!(model.state, CodexAuthState::Unauthenticated);
    assert_eq!(model.credential_account, "codex");
    assert!(!model.has_backend());
    assert!(!model.is_authenticating());
}

#[test]
fn start_without_backend_fails_closed() {
    // Given: backend が未設定。
    let mut model = CodexAuthModel::default();
    // When: ログイン開始。
    model.start();
    // Then: 明示的に失敗し、未処理イベントはない。
    assert_eq!(
        model.state,
        CodexAuthState::Failed {
            failure: CodexAuthError::Unavailable
        }
    );
    assert!(!model.poll());
}

#[test]
fn start_shows_prompt_then_authenticated_when_backend_succeeds() {
    // Given: 即時成功する backend。
    let backend = ScriptedCodexAuthBackend::immediate(Ok(CodexAuthSummary {
        expires_at_unix: Some(1_893_456_000),
    }));
    let mut model = CodexAuthModel::with_backend(backend, "personal");
    // When: ログインを開始して結果を処理する。
    model.start();
    assert_eq!(
        model.state,
        CodexAuthState::Authenticating {
            prompt: None,
            opened_browser: false
        }
    );
    poll_until(&mut model, |state| {
        matches!(state, CodexAuthState::Authenticated { .. })
    });
    // Then: 有効期限付きの認証状態になる。
    assert_eq!(
        model.state,
        CodexAuthState::Authenticated {
            expires_at_unix: Some(1_893_456_000)
        }
    );
    assert!(!model.poll());
    assert_eq!(model.credential_account, "personal");
}

#[test]
fn start_is_ignored_while_authenticating() {
    // Given: 完了を制御できる backend。
    let (backend, gate) = ScriptedCodexAuthBackend::gated();
    let mut model = CodexAuthModel::with_backend(backend.clone(), "codex");
    // When: 連続で開始する。
    model.start();
    model.start();
    poll_until(&mut model, |state| {
        matches!(
            state,
            CodexAuthState::Authenticating {
                prompt: Some(_),
                ..
            }
        )
    });
    // Then: コードが表示され、認証は一度だけ実行される。
    assert_eq!(
        model.state,
        CodexAuthState::Authenticating {
            prompt: Some(ScriptedCodexAuthBackend::prompt()),
            opened_browser: false,
        }
    );
    assert_eq!(backend.authenticate_calls(), 1);
    let before = model.state.clone();
    model.refresh_from_store();
    assert_eq!(model.state, before);
    gate.send(Ok(CodexAuthSummary::default()))
        .expect("release gate");
    poll_until(&mut model, |state| {
        matches!(state, CodexAuthState::Authenticated { .. })
    });
}

#[test]
fn failure_surfaces_category_and_allows_restart() {
    // Given: 認証が失敗する backend。
    let backend = ScriptedCodexAuthBackend::immediate(Err(CodexAuthError::Rejected));
    let mut model = CodexAuthModel::with_backend(backend.clone(), "codex");
    model.start();
    poll_until(&mut model, |state| {
        matches!(state, CodexAuthState::Failed { .. })
    });
    assert_eq!(
        model.state,
        CodexAuthState::Failed {
            failure: CodexAuthError::Rejected
        }
    );
    // When: 再度ログインを開始する。
    model.start();
    poll_until(&mut model, |state| {
        matches!(state, CodexAuthState::Failed { .. })
    });
    // Then: backend が再実行される。
    assert_eq!(backend.authenticate_calls(), 2);
}

#[test]
fn refresh_from_store_maps_summary_error_and_absence() {
    // Given: 保存済み認証情報。
    let backend = ScriptedCodexAuthBackend::authenticated(CodexAuthSummary {
        expires_at_unix: Some(42),
    });
    let mut model = CodexAuthModel::with_backend(backend.clone(), "codex");
    assert_eq!(
        model.state,
        CodexAuthState::Authenticated {
            expires_at_unix: Some(42)
        }
    );
    // When / Then: ストアの削除と読み取り失敗を状態へ反映する。
    backend.set_stored(Ok(None));
    model.refresh_from_store();
    assert_eq!(model.state, CodexAuthState::Unauthenticated);
    backend.set_stored(Err(CodexAuthError::StoreUnavailable));
    model.refresh_from_store();
    assert_eq!(
        model.state,
        CodexAuthState::Failed {
            failure: CodexAuthError::StoreUnavailable
        }
    );
}

#[test]
fn disconnected_channel_becomes_failed() {
    // Given: 結果なしで閉じたチャネル。
    let mut model = CodexAuthModel::default();
    let (tx, rx) = channel();
    model.set_receiver_for_test(rx);
    drop(tx);
    // When / Then: 切断をエラー状態として通知する。
    assert!(model.poll());
    assert_eq!(
        model.state,
        CodexAuthState::Failed {
            failure: CodexAuthError::Unavailable
        }
    );
    assert!(!model.poll());
}

#[test]
fn format_expiry_reports_remaining_time_and_expiry() {
    // Given / When / Then: 秒単位の期限を残り時間または失効へ変換する。
    assert_eq!(format_expiry(0, 3600), "Access token expires in 1h 0m");
    assert_eq!(format_expiry(0, 90), "Access token expires in 1m");
    for expiry in [50, 100] {
        assert_eq!(
            format_expiry(100, expiry),
            "Access token expired; it is refreshed automatically on the next request"
        );
    }
}

#[test]
fn debug_output_contains_no_channel_payload() {
    // Given: コードを持つ状態と未処理チャネル。
    let mut model = CodexAuthModel::default();
    let (tx, rx) = channel();
    tx.send(CodexAuthEvent::Prompt(ScriptedCodexAuthBackend::prompt()))
        .expect("prompt");
    model.set_receiver_for_test(rx);
    model.state = CodexAuthState::Authenticating {
        prompt: Some(ScriptedCodexAuthBackend::prompt()),
        opened_browser: false,
    };
    // When: Debug 表示。
    let debug = format!("{model:?}");
    // Then: チャネルやコードの実体を露出しない。
    assert!(!debug.contains("Receiver"));
    assert!(!debug.contains("ABCD-1234"));
}
