//! A bubblewrap sandbox for running one prebuilt program that needs nothing
//! from the host (the `run_js` worker, ADR 0039).
//!
//! The shell sandbox in [`crate::bwrap`] shows the system directories, the
//! project and tool caches, and forwards `PATH` / `TERM` / `LANG`, because
//! commands need them. This one is separate so that neither side has to weaken
//! for the other. The program sees only:
//!
//! - its own executable, its ELF interpreter and the shared libraries the
//!   loader reports for it (each file, read-only, at its own path);
//! - a private `/tmp` (tmpfs), `/dev` and a `/proc` of its own PID namespace;
//! - the environment variables named in `forward_env` and nothing else.
//!
//! There is no network, no other host path, no IPC or UTS namespace of the
//! host, and no capability.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bwrap::{locate_bwrap, probe};
use crate::error::SandboxError;
use crate::exec::{CommandSpec, Sandbox, WrappedCommand};

/// See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct MinimalSandbox {
    bwrap: PathBuf,
    mounts: Vec<PathBuf>,
    env: Vec<(String, String)>,
}

impl MinimalSandbox {
    /// Prepares a sandbox for `executable`, found by asking the dynamic loader
    /// (with the forwarded variables) which libraries it needs. `forward_env`
    /// names the only host variables the program receives; a development
    /// build, for instance, finds its C++ runtime through `LD_LIBRARY_PATH`.
    ///
    /// # Errors
    /// Bubblewrap is missing or unusable, or the executable's libraries cannot
    /// be listed. There is no fallback to running without the sandbox.
    pub fn detect(executable: &Path, forward_env: &[&str]) -> Result<Self, SandboxError> {
        let bwrap = locate_bwrap().ok_or_else(|| SandboxError::BwrapUnavailable {
            detail: "bwrap 実行ファイルが見つかりません".to_owned(),
        })?;
        probe(&bwrap)?;
        let env: Vec<(String, String)> = forward_env
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_owned(), value))
            })
            .collect();
        let mounts = required_files(executable, &env)?;
        Ok(Self { bwrap, mounts, env })
    }

    /// Every host file the program can see, besides `/tmp`, `/dev` and `/proc`.
    pub fn mounts(&self) -> &[PathBuf] {
        &self.mounts
    }

    /// The bubblewrap arguments for `spec`; the working directory is always `/tmp`.
    pub fn build_argv(&self, spec: &CommandSpec) -> Vec<String> {
        let mut args: Vec<String> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--cap-drop",
            "ALL",
            "--tmpfs",
            "/tmp",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
        ]
        .map(str::to_owned)
        .into();
        for path in &self.mounts {
            let path = path.to_string_lossy().into_owned();
            args.extend(["--ro-bind".to_owned(), path.clone(), path]);
        }
        args.extend(["--chdir".to_owned(), "/tmp".to_owned()]);
        args.push(spec.program.clone());
        args.extend(spec.args.iter().cloned());
        args
    }
}

impl Sandbox for MinimalSandbox {
    fn wrap(&self, spec: CommandSpec) -> Result<WrappedCommand, SandboxError> {
        let env = self
            .env
            .iter()
            .filter(|(name, _)| !spec.extra_env.iter().any(|(extra, _)| extra == name))
            .cloned()
            .chain(spec.extra_env.iter().cloned())
            .collect();
        Ok(WrappedCommand {
            program: self.bwrap.to_string_lossy().into_owned(),
            args: self.build_argv(&spec),
            cwd: None,
            inherit_env: false,
            env,
        })
    }
}

/// The executable plus what its loader needs, as absolute paths.
fn required_files(
    executable: &Path,
    env: &[(String, String)],
) -> Result<Vec<PathBuf>, SandboxError> {
    let unavailable = |detail: String| SandboxError::BwrapUnavailable { detail };
    let executable = executable
        .canonicalize()
        .map_err(|error| unavailable(format!("{}: {error}", executable.display())))?;
    let mut files = BTreeSet::from([executable.clone()]);
    if let Some(interpreter) = elf_interpreter(&executable)
        .map_err(|error| unavailable(format!("{}: {error}", executable.display())))?
    {
        files.insert(interpreter.clone());
        let mut command = Command::new(&interpreter);
        command.arg("--list").arg(&executable).env_clear();
        command.envs(env.iter().cloned());
        let output = command
            .output()
            .map_err(|error| unavailable(format!("{}: {error}", interpreter.display())))?;
        if !output.status.success() {
            return Err(unavailable(format!(
                "{} --list {} failed: {}",
                interpreter.display(),
                executable.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some(path) = listed_path(line) {
                files.insert(path);
            }
        }
    }
    Ok(files.into_iter().collect())
}

/// `\tlibc.so.6 => /lib/libc.so.6 (0x..)` or `\t/lib64/ld-linux.so.2 (0x..)`.
fn listed_path(line: &str) -> Option<PathBuf> {
    let target = match line.split_once("=>") {
        Some((_, rest)) => rest.trim(),
        None => line.trim(),
    };
    let path = target.split_whitespace().next()?;
    path.starts_with('/').then(|| PathBuf::from(path))
}

fn little_endian(bytes: &[u8]) -> u64 {
    bytes.iter().enumerate().fold(0, |value, (index, byte)| {
        value | (u64::from(*byte) << (8 * index))
    })
}

/// `PT_INTERP` of a 64-bit little-endian ELF file; `None` for a static one.
fn elf_interpreter(path: &Path) -> Result<Option<PathBuf>, String> {
    use std::io::{Read, Seek, SeekFrom};
    const PT_INTERP: u64 = 3;
    let describe = |error: std::io::Error| error.to_string();
    let mut file = std::fs::File::open(path).map_err(describe)?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).map_err(describe)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err("not a 64-bit little-endian ELF executable".to_owned());
    }
    let table = little_endian(&header[0x20..0x28]);
    let entry_size = little_endian(&header[0x36..0x38]);
    let count = little_endian(&header[0x38..0x3a]);
    let mut entry = vec![0u8; usize::try_from(entry_size).unwrap_or(0).max(56)];
    for index in 0..count {
        file.seek(SeekFrom::Start(table + index * entry_size))
            .and_then(|_| file.read_exact(&mut entry))
            .map_err(describe)?;
        if little_endian(&entry[0..4]) == PT_INTERP {
            let offset = little_endian(&entry[8..16]);
            let size = usize::try_from(little_endian(&entry[32..40])).unwrap_or(0);
            let mut name = vec![0u8; size.min(4096)];
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut name))
                .map_err(describe)?;
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len());
            return Ok(Some(PathBuf::from(
                String::from_utf8_lossy(&name[..end]).into_owned(),
            )));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_listing_lines_yield_the_files_to_mount() {
        assert_eq!(
            listed_path("\tlibc.so.6 => /nix/store/x/lib/libc.so.6 (0x00007f)"),
            Some(PathBuf::from("/nix/store/x/lib/libc.so.6"))
        );
        assert_eq!(
            listed_path("\t/lib64/ld-linux-x86-64.so.2 (0x00007f)"),
            Some(PathBuf::from("/lib64/ld-linux-x86-64.so.2"))
        );
        assert_eq!(listed_path("\tlinux-vdso.so.1 (0x00007f)"), None);
        assert_eq!(listed_path("\tlibgone.so => not found"), None);
    }

    #[test]
    fn arguments_mount_only_the_listed_files_and_a_private_tmp() {
        let sandbox = MinimalSandbox {
            bwrap: PathBuf::from("bwrap"),
            mounts: vec![
                PathBuf::from("/opt/evorch"),
                PathBuf::from("/lib/libc.so.6"),
            ],
            env: Vec::new(),
        };
        let args = sandbox.build_argv(&CommandSpec {
            program: "/opt/evorch".into(),
            args: vec!["js-worker".into()],
            cwd: Some(PathBuf::from("/ignored")),
            extra_env: Vec::new(),
        });
        let binds: Vec<_> = args
            .windows(3)
            .filter(|window| window[0] == "--ro-bind")
            .map(|window| (window[1].as_str(), window[2].as_str()))
            .collect();
        assert_eq!(
            binds,
            [
                ("/opt/evorch", "/opt/evorch"),
                ("/lib/libc.so.6", "/lib/libc.so.6")
            ]
        );
        for required in ["--unshare-net", "--unshare-pid", "--unshare-ipc", "--tmpfs"] {
            assert!(args.iter().any(|arg| arg == required), "{required}");
        }
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--bind" || arg == "--bind-try")
        );
        assert_eq!(&args[args.len() - 2..], ["/opt/evorch", "js-worker"]);
    }

    #[test]
    fn the_environment_is_exactly_the_forwarded_variables() {
        let sandbox = MinimalSandbox {
            bwrap: PathBuf::from("bwrap"),
            mounts: Vec::new(),
            env: vec![("LD_LIBRARY_PATH".into(), "/lib".into())],
        };
        let wrapped = sandbox
            .wrap(CommandSpec {
                program: "p".into(),
                args: Vec::new(),
                cwd: None,
                extra_env: Vec::new(),
            })
            .unwrap();
        assert!(!wrapped.inherit_env);
        assert_eq!(
            wrapped.env,
            [("LD_LIBRARY_PATH".to_owned(), "/lib".to_owned())]
        );
    }
}
