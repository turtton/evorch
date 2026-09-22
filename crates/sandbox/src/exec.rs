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

impl WrappedCommand {
    /// Diagnose deterministic launch failures before entering a model/tool retry
    /// loop. This does not execute a probe or disable sandboxing.
    pub fn preflight(&self) -> Result<(), SandboxError> {
        use std::os::unix::fs::PermissionsExt;
        let cwd = self
            .cwd
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .map_err(|error| SandboxError::InvalidSpec {
                detail: format!("working directory unavailable: {error}; fix cwd before retrying"),
            })?;
        if !cwd.is_dir() {
            return Err(SandboxError::InvalidSpec {
                detail: format!(
                    "working directory does not exist or is not a directory: {}; fix cwd before retrying",
                    cwd.display()
                ),
            });
        }
        let executable = |path: &std::path::Path| {
            path.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        };
        let program = std::path::Path::new(&self.program);
        let found = if self.program.contains('/') {
            executable(&if program.is_absolute() {
                program.to_path_buf()
            } else {
                cwd.join(program)
            })
        } else {
            let path = self
                .env
                .iter()
                .find(|(name, _)| name == "PATH")
                .map_or("/bin:/usr/bin", |(_, value)| value.as_str());
            std::env::split_paths(path).any(|dir| {
                executable(&if dir.is_absolute() {
                    dir.join(program)
                } else {
                    cwd.join(dir).join(program)
                })
            })
        };
        if !found {
            return Err(SandboxError::InvalidSpec {
                detail: format!(
                    "executable unavailable: {}; install it or correct PATH before retrying; sandbox fallback is disabled",
                    self.program
                ),
            });
        }
        Ok(())
    }
}

/// コマンドを実行環境へ包む境界。
pub trait Sandbox: Send + Sync {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError>;
}

/// OS 隔離を明示的に無効化する実行方式。
///
/// この型は公開 API 上の unit-like な value として構築できません。隔離の
/// 無効化は [`DirectSandbox::new_unchecked`] による明示的な opt-out
/// （非 production / テスト専用）、または審査済みの
/// [`crate::composition::unsandboxed`] 経由でのみ行えます。これは ADR 0021 の
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

#[cfg(test)]
mod preflight_tests {
    use super::*;

    #[test]
    fn missing_executable_and_invalid_cwd_report_actionable_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = WrappedCommand {
            program: "missing-evorch-executable".into(),
            args: vec![],
            cwd: Some(dir.path().into()),
            env: vec![("PATH".into(), dir.path().display().to_string())],
        };
        let error = command.preflight().unwrap_err().to_string();
        assert!(error.contains("correct PATH before retrying"));
        assert!(error.contains("sandbox fallback is disabled"));
        command.cwd = Some(dir.path().join("absent"));
        assert!(
            command
                .preflight()
                .unwrap_err()
                .to_string()
                .contains("fix cwd before retrying")
        );
    }

    #[test]
    fn executable_lookup_uses_child_path_and_mode_bits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("command");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        let command = WrappedCommand {
            program: "command".into(),
            args: vec![],
            cwd: Some(dir.path().into()),
            env: vec![("PATH".into(), ".".into())],
        };
        assert!(command.preflight().is_err());
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(command.preflight().is_ok());
    }
}
