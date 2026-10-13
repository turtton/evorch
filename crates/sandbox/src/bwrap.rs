//! bubblewrap による Linux コマンド隔離を提供します。

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use crate::{
    boundary::SandboxBoundary,
    error::SandboxError,
    exec::{CommandSpec, Sandbox, WrappedCommand, merge_environment},
};

/// bubblewrap のマウントとネットワーク構成。
#[derive(Debug, Clone)]
pub struct BwrapConfig {
    boundary: SandboxBoundary,
    allow_network: bool,
    isolate_processes: bool,
}

impl BwrapConfig {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self::from_boundary(SandboxBoundary::new(workspace_root))
    }

    /// Mount exactly `boundary`; file tools check paths against the same value.
    pub const fn from_boundary(boundary: SandboxBoundary) -> Self {
        Self {
            boundary,
            allow_network: false,
            isolate_processes: false,
        }
    }

    pub const fn boundary(&self) -> &SandboxBoundary {
        &self.boundary
    }

    /// Workspace used for relative file paths as well as sandbox commands.
    pub fn workspace_root(&self) -> &Path {
        self.boundary.workspace_root()
    }

    /// Mount only registry / git caches and tool binaries, never Cargo credentials
    /// or host config. Cargo's root and lock files remain in the private HOME.
    pub fn cargo_cache(mut self, path: impl Into<PathBuf>) -> Self {
        self.boundary = self.boundary.cargo_cache(path);
        self
    }

    pub const fn allow_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    /// Hide host processes and terminate sandbox descendants when its PID
    /// namespace exits. Required when host-side files must remain inaccessible
    /// through `/proc/<pid>/root` or `/proc/<pid>/cwd`.
    pub const fn isolate_processes(mut self) -> Self {
        self.isolate_processes = true;
        self
    }

    pub fn ro_bind(mut self, path: impl Into<PathBuf>) -> Self {
        self.boundary = self.boundary.read_only(path);
        self
    }

    pub fn rw_bind(mut self, path: impl Into<PathBuf>) -> Self {
        self.boundary = self.boundary.read_write(path);
        self
    }

    /// Use `path` as this sandbox's `TMPDIR`, visible at the same path and at `/tmp`.
    /// Without one, each sandbox gets a private directory removed when it drops.
    pub fn scratch_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.boundary = self.boundary.with_scratch(path);
        self
    }
}

/// 機能確認済み bubblewrap 実行方式。
#[derive(Debug, Clone)]
pub struct BwrapSandbox {
    program: PathBuf,
    config: BwrapConfig,
    scratch: PathBuf,
    _owned_scratch: Option<Arc<crate::scratch::Scratch>>,
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
        let unavailable = |error: std::io::Error| SandboxError::BwrapUnavailable {
            detail: format!("/var/tmp の作業領域を作成できません: {error}"),
        };
        let (scratch, owned) = match &config.boundary.scratch {
            Some(path) => {
                crate::scratch::ensure_layout(path).map_err(unavailable)?;
                (path.clone(), None)
            }
            None => {
                let owned = crate::scratch::Scratch::new().map_err(unavailable)?;
                (owned.path().to_path_buf(), Some(Arc::new(owned)))
            }
        };
        Ok(Self {
            program: program.to_path_buf(),
            config,
            scratch,
            _owned_scratch: owned,
        })
    }

    pub fn build_argv(&self, spec: &CommandSpec) -> Vec<String> {
        let mut args = vec!["--die-with-parent".to_owned()];
        if self.config.isolate_processes {
            args.push("--unshare-pid".to_owned());
        }
        // The same directory at its host path lets file tools and unsandboxed
        // shells resolve `$TMPDIR` paths; `/tmp` serves programs that hardcode it.
        let scratch = self.scratch.to_string_lossy().into_owned();
        args.extend(["--bind".to_owned(), scratch.clone(), "/tmp".to_owned()]);
        args.extend(["--bind".to_owned(), scratch.clone(), scratch]);
        let boundary = &self.config.boundary;
        for path in &boundary.read_only {
            if let Some(parent) = path.parent()
                && parent != Path::new("/")
                && boundary.workspace_root.starts_with(parent)
            {
                let parent = parent.to_string_lossy().into_owned();
                args.extend(["--ro-bind-try".to_owned(), parent.clone(), parent]);
            }
        }
        for path in &boundary.read_only {
            let path = path.to_string_lossy().into_owned();
            args.extend(["--ro-bind-try".to_owned(), path.clone(), path]);
        }
        self.append_build_cache_mounts(&mut args);
        for path in &boundary.read_write {
            let path = path.to_string_lossy().into_owned();
            args.extend(["--bind".to_owned(), path.clone(), path]);
        }
        let workspace = boundary.workspace_root.to_string_lossy().into_owned();
        args.extend(["--bind".to_owned(), workspace.clone(), workspace.clone()]);
        args.extend(["--dev".to_owned(), "/dev".to_owned()]);
        args.extend(["--proc".to_owned(), "/proc".to_owned()]);
        if !self.config.allow_network {
            args.push("--unshare-net".to_owned());
        }
        let cwd = match spec.cwd.as_ref() {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => boundary.workspace_root.join(path),
            None => boundary.workspace_root.clone(),
        };
        args.extend(["--chdir".to_owned(), cwd.to_string_lossy().into_owned()]);
        args.push(spec.program.clone());
        args.extend(spec.args.iter().cloned());
        args
    }

    fn append_build_cache_mounts(&self, args: &mut Vec<String>) {
        if self.config.isolate_processes
            && let Some(home) = self.config.boundary.rustup_home()
            && home.is_absolute()
        {
            // Nix-patched linker scripts embed their original toolchain
            // path. Preserve that read-only alias without exposing the
            // original HOME, Rustup settings or Cargo credentials.
            let toolchains = home.join("toolchains");
            bind_cache(args, &toolchains, &toolchains);
        }
        for mount in self.config.boundary.cache_mounts() {
            bind_cache(args, &mount.host, &mount.sandbox);
        }
    }
}

fn bind_cache(args: &mut Vec<String>, source: &Path, destination: &Path) {
    if source.exists() {
        args.extend([
            "--ro-bind".to_owned(),
            source.to_string_lossy().into_owned(),
            destination.to_string_lossy().into_owned(),
        ]);
    }
}

impl Sandbox for BwrapSandbox {
    fn with_network_access(&self) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut networked = self.clone();
        networked.config.allow_network = true;
        Ok(Arc::new(networked))
    }

    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        // An ownership transition may have emptied a shared directory since detection.
        crate::scratch::ensure_layout(&self.scratch).map_err(|error| {
            SandboxError::BwrapUnavailable {
                detail: format!("/var/tmp の作業領域を作成できません: {error}"),
            }
        })?;
        let args = self.build_argv(&spec);
        let mut env = merge_environment(spec.extra_env);
        env.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                "HOME" | "TMPDIR" | "CARGO_HOME" | "RUSTUP_HOME"
            )
        });
        env.extend([
            ("HOME".to_owned(), "/tmp/home".to_owned()),
            (
                "TMPDIR".to_owned(),
                self.scratch.to_string_lossy().into_owned(),
            ),
            ("CARGO_HOME".to_owned(), "/tmp/home/.cargo".to_owned()),
            ("RUSTUP_HOME".to_owned(), "/tmp/home/.rustup".to_owned()),
        ]);
        let mut fallback = "/tmp/home/.cargo/bin".to_owned();
        if self.config.isolate_processes {
            fallback.push_str(":/usr/bin:/bin");
            // Nix system profiles live under unmounted /run. Their canonical
            // store paths are already read-only mounted, including bash needed
            // by compiler/linker wrappers. No additional host mount is needed.
            if let Ok(system_bin) = Path::new("/run/current-system/sw/bin").canonicalize()
                && system_bin.starts_with("/nix/store")
            {
                fallback.push(':');
                fallback.push_str(&system_bin.to_string_lossy());
            }
        }
        if let Some((_, path)) = env.iter_mut().find(|(key, _)| key == "PATH") {
            // Preserve host-selected compiler priority (notably Nix wrappers).
            // Rustup and mounted system binaries supply missing host paths.
            path.push(':');
            path.push_str(&fallback);
        } else if self.config.isolate_processes {
            env.push(("PATH".to_owned(), fallback));
        }
        Ok(WrappedCommand {
            program: self.program.to_string_lossy().into_owned(),
            args,
            cwd: None,
            inherit_env: false,
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
    use crate::boundary::DEFAULT_RO_BINDS;

    fn sandbox(config: BwrapConfig) -> BwrapSandbox {
        let owned = Arc::new(crate::scratch::Scratch::new().unwrap());
        BwrapSandbox {
            program: PathBuf::from("bwrap"),
            config,
            scratch: owned.path().to_path_buf(),
            _owned_scratch: Some(owned),
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
        assert!(argv.windows(3).any(|args| args[0] == "--bind"
            && args[1].starts_with("/var/tmp/evorch-scratch-")
            && args[2] == "/tmp"));
        assert!(argv.windows(3).any(|args| args[0] == "--bind"
            && args[1].starts_with("/var/tmp/evorch-scratch-")
            && args[2] == args[1]));
        assert!(!argv.iter().any(|arg| arg == "--tmpfs"));
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

    #[test]
    fn private_processes_are_opt_in_and_preserved_with_network_access() {
        let rustup = tempfile::tempdir().unwrap();
        let toolchains = rustup.path().join("toolchains");
        std::fs::create_dir(&toolchains).unwrap();
        std::fs::write(rustup.path().join("settings.toml"), "private settings").unwrap();
        let mut config = BwrapConfig::new(PathBuf::from("/workspace"));
        config.boundary.rustup_home = Some(rustup.path().to_path_buf());
        let default = sandbox(config.clone());
        let mut command = spec(None);
        command.extra_env = vec![(
            "PATH".into(),
            "/preferred/bin:/run/current-system/sw/bin".into(),
        )];
        let path = |wrapped: &WrappedCommand| {
            wrapped
                .env
                .iter()
                .find(|(key, _)| key == "PATH")
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(
            path(&default.wrap(command.clone()).unwrap()),
            "/preferred/bin:/run/current-system/sw/bin:/tmp/home/.cargo/bin"
        );
        assert!(
            !default
                .build_argv(&spec(None))
                .iter()
                .any(|arg| arg == "--unshare-pid")
        );
        let toolchains = toolchains.to_string_lossy().into_owned();
        assert!(!default.build_argv(&command).windows(3).any(|args| {
            args[0] == "--ro-bind" && args[1] == toolchains && args[2] == toolchains
        }));
        let isolated = sandbox(config.isolate_processes());
        for boundary in [
            isolated.wrap(command.clone()).unwrap(),
            isolated
                .with_network_access()
                .unwrap()
                .wrap(command)
                .unwrap(),
        ] {
            assert!(boundary.args.iter().any(|arg| arg == "--unshare-pid"));
            assert!(boundary.args.windows(3).any(|args| {
                args[0] == "--ro-bind" && args[1] == toolchains && args[2] == toolchains
            }));
            for forbidden in [
                rustup.path().to_path_buf(),
                rustup.path().join("settings.toml"),
            ] {
                assert!(!boundary.args.windows(3).any(|args| {
                    args[0] == "--ro-bind" && args[2] == forbidden.to_string_lossy()
                }));
            }
            assert!(path(&boundary).starts_with(
                "/preferred/bin:/run/current-system/sw/bin:/tmp/home/.cargo/bin:/usr/bin:/bin"
            ));
        }
    }

    #[test]
    fn network_only_variant_preserves_every_other_boundary() {
        let sandbox = sandbox(
            BwrapConfig::new(PathBuf::from("/workspace"))
                .ro_bind("/opt/readonly")
                .rw_bind("/opt/writable"),
        );
        let command = spec(Some(PathBuf::from("/workspace/sub")));
        let original = sandbox.wrap(command.clone()).expect("isolated wrap");
        let network = sandbox
            .with_network_access()
            .expect("network variant")
            .wrap(command)
            .expect("network wrap");
        assert!(original.args.contains(&"--unshare-net".to_owned()));
        assert!(!network.args.contains(&"--unshare-net".to_owned()));
        assert_eq!(
            network.args,
            original
                .args
                .into_iter()
                .filter(|arg| arg != "--unshare-net")
                .collect::<Vec<_>>()
        );
        assert_eq!(network.program, original.program);
        assert_eq!(network.env, original.env);
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

    // Given: a boundary with every region kind / When: building bwrap argv from it /
    // Then: every region is mounted and every host-path mount is a listed region
    #[test]
    fn bwrap_mounts_exactly_the_boundary_regions() {
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("registry/src")).unwrap();
        std::fs::create_dir_all(cache.path().join("bin")).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut boundary = SandboxBoundary::new(PathBuf::from("/workspace"))
            .cargo_cache(cache.path())
            .read_only("/opt/readonly")
            .read_write("/opt/writable")
            .with_scratch(scratch.path());
        boundary.rustup_home = None;
        let config = BwrapConfig::from_boundary(boundary);
        let regions = config.boundary().regions();
        let sandbox = BwrapSandbox {
            program: PathBuf::from("bwrap"),
            scratch: scratch.path().to_path_buf(),
            _owned_scratch: None,
            config,
        };
        let argv = sandbox.build_argv(&spec(None));
        let mounts = argv
            .windows(3)
            .filter(|args| matches!(args[0].as_str(), "--bind" | "--ro-bind" | "--ro-bind-try"))
            .map(|args| (args[0].clone(), args[1].clone(), args[2].clone()))
            .collect::<Vec<_>>();
        for region in &regions {
            let host = region.path.to_string_lossy();
            let mounted = mounts.iter().any(|(flag, source, _)| {
                *source == host
                    && match region.access {
                        crate::PathAccess::ReadWrite => flag == "--bind",
                        crate::PathAccess::ReadOnly => flag != "--bind",
                    }
            });
            let missing_cache = !region.path.exists() && region.path.starts_with(cache.path());
            assert!(mounted || missing_cache, "{host} is not mounted: {argv:?}");
        }
        for (_, source, _) in &mounts {
            assert!(
                regions
                    .iter()
                    .any(|region| region.path == Path::new(source)),
                "{source} is mounted but not in the boundary"
            );
        }
    }

    // Given: a configured scratch directory / When: wrapping / Then: it is bound at its host
    // path and at /tmp, TMPDIR names the host path, and HOME and caches stay under /tmp/home
    #[test]
    fn configured_scratch_is_shared_by_host_path_and_tmp() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("thread");
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("registry/src")).unwrap();
        let mut config = BwrapConfig::new(PathBuf::from("/workspace"))
            .cargo_cache(cache.path())
            .scratch_dir(&path);
        config.boundary.rustup_home = None;
        let sandbox = BwrapSandbox {
            program: PathBuf::from("bwrap"),
            scratch: path.clone(),
            _owned_scratch: None,
            config,
        };
        let wrapped = sandbox.wrap(spec(None)).unwrap();
        assert!(path.join("home/.cargo").is_dir());
        let host = path.to_string_lossy().into_owned();
        let tmp = wrapped
            .args
            .windows(3)
            .position(|args| args == ["--bind", host.as_str(), "/tmp"])
            .unwrap();
        let same = wrapped
            .args
            .windows(3)
            .position(|args| args == ["--bind", host.as_str(), host.as_str()])
            .unwrap();
        let cache_mount = wrapped
            .args
            .windows(3)
            .position(|args| args[0] == "--ro-bind" && args[2] == "/tmp/home/.cargo/registry/src")
            .unwrap();
        assert!(tmp < cache_mount && same < cache_mount);
        for (key, value) in [
            ("TMPDIR", host.as_str()),
            ("HOME", "/tmp/home"),
            ("CARGO_HOME", "/tmp/home/.cargo"),
            ("RUSTUP_HOME", "/tmp/home/.rustup"),
        ] {
            assert!(
                wrapped.env.contains(&(key.to_owned(), value.to_owned())),
                "{key}: {:?}",
                wrapped.env
            );
        }
        let networked = sandbox
            .with_network_access()
            .unwrap()
            .wrap(spec(None))
            .unwrap();
        assert!(networked.env.contains(&("TMPDIR".to_owned(), host.clone())));
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
