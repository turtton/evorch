//! bubblewrap による Linux コマンド隔離を提供します。

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    error::SandboxError,
    exec::{CommandSpec, Sandbox, WrappedCommand, merge_environment},
};

const DEFAULT_RO_BINDS: [&str; 6] = ["/usr", "/bin", "/lib", "/lib64", "/etc", "/nix"];

/// bubblewrap のマウントとネットワーク構成。
#[derive(Debug, Clone)]
pub struct BwrapConfig {
    workspace_root: PathBuf,
    allow_network: bool,
    ro_binds: Vec<PathBuf>,
    rw_binds: Vec<PathBuf>,
}

impl BwrapConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            allow_network: false,
            ro_binds: DEFAULT_RO_BINDS.into_iter().map(PathBuf::from).collect(),
            rw_binds: Vec::new(),
        }
    }

    pub const fn allow_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    pub fn ro_bind(mut self, path: impl Into<PathBuf>) -> Self {
        self.ro_binds.push(path.into());
        self
    }

    pub fn rw_bind(mut self, path: impl Into<PathBuf>) -> Self {
        self.rw_binds.push(path.into());
        self
    }
}

/// 機能確認済み bubblewrap 実行方式。
#[derive(Debug, Clone)]
pub struct BwrapSandbox {
    program: PathBuf,
    config: BwrapConfig,
}

impl BwrapSandbox {
    pub fn detect(config: BwrapConfig) -> Result<Self, SandboxError> {
        let program = locate_bwrap().ok_or_else(|| SandboxError::BwrapUnavailable {
            detail: "bwrap 実行ファイルが見つかりません".to_owned(),
        })?;
        Self::detect_with_program(&program, config)
    }

    pub fn detect_with_program(program: &Path, config: BwrapConfig) -> Result<Self, SandboxError> {
        let status = Command::new(program)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-net",
                "--die-with-parent",
                "true",
            ])
            .status()
            .map_err(|error| SandboxError::BwrapUnavailable {
                detail: error.to_string(),
            })?;
        if !status.success() {
            return Err(SandboxError::BwrapUnavailable {
                detail: format!("機能確認が終了コード {status} で失敗しました"),
            });
        }
        Ok(Self {
            program: program.to_path_buf(),
            config,
        })
    }

    pub fn build_argv(&self, spec: &CommandSpec) -> Vec<String> {
        let mut args = vec!["--die-with-parent".to_owned()];
        args.extend(["--tmpfs".to_owned(), "/tmp".to_owned()]);
        args.extend(["--dir".to_owned(), "/tmp/home".to_owned()]);
        for path in &self.config.ro_binds {
            if let Some(parent) = path.parent()
                && parent != Path::new("/")
                && self.config.workspace_root.starts_with(parent)
            {
                let parent = parent.to_string_lossy().into_owned();
                args.extend(["--ro-bind-try".to_owned(), parent.clone(), parent]);
            }
        }
        for path in &self.config.ro_binds {
            let path = path.to_string_lossy().into_owned();
            args.extend(["--ro-bind-try".to_owned(), path.clone(), path]);
        }
        for path in &self.config.rw_binds {
            let path = path.to_string_lossy().into_owned();
            args.extend(["--bind".to_owned(), path.clone(), path]);
        }
        let workspace = self.config.workspace_root.to_string_lossy().into_owned();
        args.extend(["--bind".to_owned(), workspace.clone(), workspace.clone()]);
        args.extend(["--dev".to_owned(), "/dev".to_owned()]);
        args.extend(["--proc".to_owned(), "/proc".to_owned()]);
        if !self.config.allow_network {
            args.push("--unshare-net".to_owned());
        }
        let cwd = spec
            .cwd
            .as_ref()
            .unwrap_or(&self.config.workspace_root)
            .to_string_lossy()
            .into_owned();
        args.extend(["--chdir".to_owned(), cwd]);
        args.push(spec.program.clone());
        args.extend(spec.args.iter().cloned());
        args
    }
}

impl Sandbox for BwrapSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        let args = self.build_argv(&spec);
        let mut env = merge_environment(spec.extra_env);
        env.retain(|(key, _)| key != "HOME");
        env.push(("HOME".to_owned(), "/tmp/home".to_owned()));
        Ok(WrappedCommand {
            program: self.program.to_string_lossy().into_owned(),
            args,
            cwd: None,
            env,
        })
    }
}

fn locate_bwrap() -> Option<PathBuf> {
    let path = Command::new("which").arg("bwrap").output().ok()?;
    if path.status.success() {
        let program = String::from_utf8(path.stdout).ok()?;
        return Some(PathBuf::from(program.trim()));
    }
    let fallback =
        PathBuf::from("/nix/store/lqndphylsxqwbwm804n473pb4sqb98sh-bubblewrap-0.11.2/bin/bwrap");
    fallback.exists().then_some(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(config: BwrapConfig) -> BwrapSandbox {
        BwrapSandbox {
            program: PathBuf::from("bwrap"),
            config,
        }
    }

    fn spec(cwd: Option<PathBuf>) -> CommandSpec {
        CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), "true".to_owned()],
            cwd,
            extra_env: Vec::new(),
        }
    }

    // Given: 既定構成 / When: 引数を構築 / Then: 読取 bind・作業領域・一時 HOME・ネットワーク拒否が含まれる
    #[test]
    fn default_argv_contains_isolation() {
        let argv = sandbox(BwrapConfig::new(PathBuf::from("/workspace"))).build_argv(&spec(None));
        assert!(
            argv.windows(3)
                .any(|args| args == ["--bind", "/workspace", "/workspace"])
        );
        assert!(argv.windows(2).any(|args| args == ["--dir", "/tmp/home"]));
        assert!(argv.contains(&"--unshare-net".to_owned()));
        for path in DEFAULT_RO_BINDS {
            assert!(
                argv.windows(3)
                    .any(|args| args == ["--ro-bind-try", path, path])
            );
        }
    }

    // Given: ネットワーク許可構成 / When: 引数を構築 / Then: ネットワーク分離を指定しない
    #[test]
    fn allow_network_omits_unshare() {
        let argv = sandbox(BwrapConfig::new(PathBuf::from("/workspace")).allow_network(true))
            .build_argv(&spec(None));
        assert!(!argv.contains(&"--unshare-net".to_owned()));
    }

    // Given: 個別作業ディレクトリ / When: 引数を構築 / Then: 指定ディレクトリへ移動する
    #[test]
    fn spec_cwd_overrides_workspace() {
        let argv = sandbox(BwrapConfig::new(PathBuf::from("/workspace")))
            .build_argv(&spec(Some(PathBuf::from("/workspace/sub"))));
        assert!(
            argv.windows(2)
                .any(|args| args == ["--chdir", "/workspace/sub"])
        );
    }

    // Given: 追加の読み取り bind / When: 引数を構築 / Then: 追加パスが含まれる
    #[test]
    fn extra_ro_bind_is_included() {
        let argv = sandbox(BwrapConfig::new(PathBuf::from("/workspace")).ro_bind("/opt/data"))
            .build_argv(&spec(None));
        assert!(
            argv.windows(3)
                .any(|args| args == ["--ro-bind-try", "/opt/data", "/opt/data"])
        );
    }

    // Given: 追加の読み書き bind / When: 引数を構築 / Then: 必須 bind として追加パスが含まれる
    #[test]
    fn rw_bind_appends_bind_flag() {
        let argv =
            sandbox(BwrapConfig::new(PathBuf::from("/workspace")).rw_bind("/repo/.git/objects"))
                .build_argv(&spec(None));
        assert!(
            argv.windows(3)
                .any(|args| args == ["--bind", "/repo/.git/objects", "/repo/.git/objects"])
        );
    }

    // Given: 読取親 bind と読み書き子 bind / When: 引数を構築 / Then: 子 bind が親 bind を後から上書きする
    #[test]
    fn rw_binds_ordered_after_ro_binds() {
        let argv = sandbox(
            BwrapConfig::new(PathBuf::from("/workspace"))
                .ro_bind("/repo/.git")
                .rw_bind("/repo/.git/objects"),
        )
        .build_argv(&spec(None));
        let ro_bind_index = argv
            .windows(3)
            .position(|args| args == ["--ro-bind-try", "/repo/.git", "/repo/.git"])
            .expect("読み取り親 bind が含まれるはずです");
        let rw_bind_index = argv
            .windows(3)
            .position(|args| args == ["--bind", "/repo/.git/objects", "/repo/.git/objects"])
            .expect("読み書き子 bind が含まれるはずです");
        assert!(ro_bind_index < rw_bind_index);
    }

    // Given: 存在しない bwrap / When: 機能確認 / Then: 利用不可として閉じる
    #[test]
    fn missing_program_fails_closed() {
        let result = BwrapSandbox::detect_with_program(
            Path::new("/nonexistent/bwrap"),
            BwrapConfig::new(PathBuf::from("/workspace")),
        );
        assert!(matches!(result, Err(SandboxError::BwrapUnavailable { .. })));
    }

    // Given: bubblewrap 方式 / When: コマンドを包む / Then: HOME は隔離先へ固定される
    #[test]
    fn wrapped_environment_overrides_home() {
        let wrapped = sandbox(BwrapConfig::new(PathBuf::from("/workspace")))
            .wrap(spec(None))
            .expect("コマンドを包めるはずです");
        assert!(
            wrapped
                .env
                .contains(&("HOME".to_owned(), "/tmp/home".to_owned()))
        );
    }
}
