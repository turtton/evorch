//! コマンド仕様と隔離方式の共通抽象を定義します。

use std::{env, path::PathBuf};

use crate::error::SandboxError;

/// 隔離前のコマンド指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub extra_env: Vec<(String, String)>,
}

/// 実際に起動するプログラムと引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// コマンドを実行環境へ包む境界。
pub trait Sandbox: Send + Sync {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError>;
}

/// OS 隔離を明示的に無効化する実行方式。
///
/// この型は公開 API 上の unit-like な value として構築できません。隔離の
/// 無効化は [`DirectSandbox::new_unchecked`] による明示的な opt-out
/// （非 production / テスト専用）のみで行えます。これは ADR 0021 の
/// fail-closed 方針を construction API に適用したもので、policy 明示なしの
/// permissive な構築経路を module visibility で構造的に塞ぐ invariant です
/// （trybuild 等の compile-fail テストに代わり、本 doc と移行済みの
/// テストで固定します）。
#[derive(Debug, Clone, Copy)]
pub struct DirectSandbox {
    _sealed: (),
}

impl Sandbox for DirectSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        Ok(WrappedCommand {
            program: spec.program,
            args: spec.args,
            cwd: spec.cwd,
            env: merge_environment(spec.extra_env),
        })
    }
}

impl DirectSandbox {
    /// OS 隔離を無効化する明示的な opt-out constructor。
    ///
    /// 非 production / テスト専用。production の tool 実行構築は
    /// `composition::production_sandbox`（fail-closed composition root）を
    /// 使うこと。ADR 0021 の方針により、隔離なし実行はこの API のような
    /// 明示的な意図表明経由でのみ許可される。
    pub const fn new_unchecked() -> Self {
        Self { _sealed: () }
    }
}

pub(crate) fn merge_environment(extra_env: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut allowed = ["PATH", "TERM", "LANG", "LC_ALL"]
        .into_iter()
        .filter_map(|key| env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect::<Vec<_>>();
    for (key, value) in extra_env {
        if let Some(existing) = allowed.iter_mut().find(|(name, _)| name == &key) {
            existing.1 = value;
        } else {
            allowed.push((key, value));
        }
    }
    allowed
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn spec() -> CommandSpec {
        CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            cwd: Some(PathBuf::from("/workspace")),
            extra_env: vec![("CUSTOM".to_owned(), "value".to_owned())],
        }
    }

    // Given: 親環境に存在し得る秘密名 / When: 直接方式で包む / Then: 許可リスト外の環境は渡らない
    #[test]
    fn parent_secret_is_not_forwarded() {
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(spec())
            .expect("コマンドを包めるはずです");
        assert!(!wrapped.env.iter().any(|(key, _)| key == "FAKE_SECRET"));
    }

    // Given: 親 PATH / When: 直接方式で包む / Then: PATH が引き継がれる
    #[test]
    fn path_is_forwarded() {
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(spec())
            .expect("コマンドを包めるはずです");
        assert_eq!(
            wrapped
                .env
                .iter()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value.as_str()),
            std::env::var("PATH").ok().as_deref()
        );
    }

    // Given: 追加環境 / When: 直接方式で包む / Then: 指定値が統合される
    #[test]
    fn extra_environment_is_merged() {
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(spec())
            .expect("コマンドを包めるはずです");
        assert!(
            wrapped
                .env
                .contains(&("CUSTOM".to_owned(), "value".to_owned()))
        );
    }

    // Given: 作業ディレクトリ付き仕様 / When: 直接方式で包む / Then: 作業ディレクトリが保持される
    #[test]
    fn cwd_is_preserved() {
        let wrapped = DirectSandbox::new_unchecked()
            .wrap(spec())
            .expect("コマンドを包めるはずです");
        assert_eq!(wrapped.cwd, Some(PathBuf::from("/workspace")));
    }
}
