//! ツール実行のための本番用コンポジションルートを提供します。
//!
//! ADR 0021 の方針に基づき、このモジュールは fail-closed として振る舞います。
//! bwrap の検出に失敗した場合はその場で呼び出し元へエラーを伝播し、
//! サンドボックスなしでの実行へのフォールバックは一切行いません。
//! 構築される実行方式は常に `BwrapSandbox` です。

use std::sync::Arc;

use crate::{
    bwrap::{BwrapConfig, BwrapSandbox},
    error::SandboxError,
    exec::Sandbox,
};

/// ツール実行に用いる本番用サンドボックスを構築します。
///
/// 検出に失敗した場合はフォールバックせず、エラーをそのまま返します
/// (fail-closed)。
pub fn production_sandbox(config: BwrapConfig) -> Result<Arc<dyn Sandbox>, SandboxError> {
    compose_with(|| BwrapSandbox::detect(config))
}

/// bwrap 検出を注入できる、テスト用の非公開シームです。
fn compose_with(
    detect: impl FnOnce() -> Result<BwrapSandbox, SandboxError>,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    Ok(Arc::new(detect()?))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use super::*;
    use crate::exec::CommandSpec;
    use tempfile::tempdir;

    // Given: 失敗する検出シーム / When: サンドボックスを構築する / Then: エラーがそのまま伝播する
    #[test]
    fn stubbed_failure_propagates_fail_closed() {
        let result = compose_with(|| {
            Err(SandboxError::BwrapUnavailable {
                detail: "bwrap の検出に失敗".to_owned(),
            })
        });

        assert!(matches!(result, Err(SandboxError::BwrapUnavailable { .. })));
    }

    // Given: 存在しない bwrap パス / When: 実検出でサンドボックスを構築する / Then: エラーがそのまま伝播する
    #[test]
    fn missing_program_propagates_fail_closed() {
        let result = compose_with(|| {
            BwrapSandbox::detect_with_program(
                Path::new("/nonexistent/bwrap"),
                BwrapConfig::new(PathBuf::from("/workspace")),
            )
        });

        assert!(matches!(result, Err(SandboxError::BwrapUnavailable { .. })));
    }

    // Given: 機能確認を通過する疑似 bwrap / When: 構築してコマンドを包む / Then: プログラムは検出結果を使う
    #[test]
    fn wrap_uses_detected_program() {
        let dir = tempdir().expect("テンポラリディレクトリを作成できるはずです");
        let script = dir.path().join("fake-bwrap");
        fs::write(&script, "#!/bin/sh\nexit 0\n").expect("疑似 bwrap を作成できるはずです");
        // fs::write はハンドルを閉じてから返るため、後続の実行で ETXTBSY にならない。
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("疑似 bwrap に実行権限を設定できるはずです");

        let sandbox = compose_with(|| {
            BwrapSandbox::detect_with_program(
                &script,
                BwrapConfig::new(PathBuf::from("/workspace")),
            )
        })
        .expect("サンドボックスを構築できるはずです");
        let wrapped = sandbox
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec!["-c".to_owned(), "true".to_owned()],
                cwd: None,
                extra_env: Vec::new(),
            })
            .expect("コマンドを包めるはずです");

        assert_eq!(
            wrapped.program,
            script.display().to_string(),
            "ラップ結果のプログラムは検出された bwrap を使うはずです"
        );
    }
}
