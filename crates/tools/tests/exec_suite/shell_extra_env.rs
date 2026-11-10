//! shell ツールが子プロセスへ追加の環境変数を渡すことの統合テスト。

use std::sync::Arc;

use sandbox::DirectSandbox;
use serde_json::json;
use tools::{Shell, Tool};

// Given: with_env で追加環境を注入 / When: 子プロセスで環境変数を読む / Then: 値が子へ渡る
#[tokio::test]
async fn with_env_forwards_extra_env_to_child() {
    let shell = Shell::with_env(
        Arc::new(DirectSandbox::new_unchecked()),
        vec![("SHELL_ENV_PROBE".to_string(), "forwarded".to_string())],
    );

    let result = shell
        .execute(json!({
            "command": "printf %s \"$SHELL_ENV_PROBE\""
        }))
        .await
        .expect("実行に成功するはずです");

    assert!(!result.is_error);
    assert!(result.content.contains("forwarded"));
}
