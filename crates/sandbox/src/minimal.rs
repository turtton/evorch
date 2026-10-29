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
//!
//! Finding the libraries needs the executable's own dynamic loader, which this
//! module never runs on the host: the loader must be a trusted file (root-owned
//! below a standard system directory, or a Nix store path, and not writable by
//! group or others), and it runs inside its own bubblewrap sandbox without
//! network, workspace or environment, under a deadline and an output cap.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::bwrap::{locate_bwrap, probe};
use crate::error::SandboxError;
use crate::exec::{CommandSpec, Sandbox, WrappedCommand};
use crate::session::StdioSession;

/// Longest the library listing may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Most output the listing may produce.
const DISCOVERY_OUTPUT_BYTES: usize = 64 * 1024;
/// How often a listing in progress looks at the cancellation flag.
const CANCEL_POLL: Duration = Duration::from_millis(20);
/// How long the process may linger after it closed its output.
const EXIT_GRACE: Duration = Duration::from_secs(2);
/// Where a loader may live: files here are installed by the system or are content-addressed.
const SYSTEM_DIRECTORIES: [&str; 5] = ["/usr", "/lib", "/lib64", "/bin", "/sbin"];
const NIX_ROOT: &str = "/nix";
const NIX_STORE: &str = "/nix/store";

/// See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct MinimalSandbox {
    bwrap: PathBuf,
    executable: PathBuf,
    mounts: Vec<Mount>,
    env: Vec<(String, String)>,
}

/// A host file shown read-only at `dest` (which may be an alias of `source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub dest: PathBuf,
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
        Self::detect_cancellable(executable, forward_env, &AtomicBool::new(false))
    }

    /// Like [`Self::detect`], but stops as soon as `cancel` is set: a listing in
    /// progress is killed and reaped, and an error is returned instead of a sandbox.
    ///
    /// # Errors
    /// As [`Self::detect`], or the preparation was cancelled.
    pub fn detect_cancellable(
        executable: &Path,
        forward_env: &[&str],
        cancel: &AtomicBool,
    ) -> Result<Self, SandboxError> {
        let unavailable = |detail: String| SandboxError::BwrapUnavailable { detail };
        let executable = executable
            .canonicalize()
            .map_err(|error| unavailable(format!("{}: {error}", executable.display())))?;
        let requested = elf_interpreter(&executable)
            .map_err(|error| unavailable(format!("{}: {error}", executable.display())))?;
        let loader = requested
            .as_deref()
            .map(trusted_loader)
            .transpose()
            .map_err(unavailable)?;
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
        let listed = match &loader {
            Some(loader) => {
                list_libraries(&bwrap, loader, &executable, &env, cancel).map_err(unavailable)?
            }
            None => Vec::new(),
        };
        if cancel.load(Ordering::SeqCst) {
            return Err(unavailable("preparation was cancelled".to_owned()));
        }
        Ok(Self {
            bwrap,
            mounts: plan_mounts(
                &executable,
                requested.as_deref(),
                loader.as_deref(),
                &listed,
            ),
            executable,
            env,
        })
    }

    /// The resolved path of the program: the one that is mounted, and the one to run.
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    /// The paths at which the program sees host files, besides `/tmp`, `/dev` and `/proc`.
    pub fn mount_points(&self) -> Vec<PathBuf> {
        self.mounts.iter().map(|mount| mount.dest.clone()).collect()
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
        for mount in &self.mounts {
            args.extend([
                "--ro-bind".to_owned(),
                mount.source.to_string_lossy().into_owned(),
                mount.dest.to_string_lossy().into_owned(),
            ]);
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

/// The files the loader reports for `executable`, from a run of the trusted
/// `loader` inside a sandbox that shows only system directories and the executable.
fn list_libraries(
    bwrap: &Path,
    loader: &Path,
    executable: &Path,
    env: &[(String, String)],
    cancel: &AtomicBool,
) -> Result<Vec<PathBuf>, String> {
    let wrapped = WrappedCommand {
        program: bwrap.to_string_lossy().into_owned(),
        args: discovery_argv(loader, executable),
        cwd: None,
        inherit_env: false,
        env: env.to_vec(),
    };
    let listing = run_bounded(wrapped, DISCOVERY_TIMEOUT, DISCOVERY_OUTPUT_BYTES, cancel)
        .map_err(|error| format!("listing the libraries of {}: {error}", executable.display()))?;
    let files: Vec<PathBuf> = listing.lines().filter_map(listed_path).collect();
    if files.is_empty() {
        return Err(format!(
            "{} listed no libraries for {}",
            loader.display(),
            executable.display()
        ));
    }
    Ok(files)
}

/// What the program sees: itself, the checked loader (also at the path the ELF
/// asks for, which on a merged-usr system is an alias such as `/lib64/ld-linux-…`),
/// and the libraries the loader listed.
fn plan_mounts(
    executable: &Path,
    requested_loader: Option<&Path>,
    loader: Option<&Path>,
    listed: &[PathBuf],
) -> Vec<Mount> {
    let mut by_dest: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    by_dest.insert(executable.to_path_buf(), executable.to_path_buf());
    if let Some(loader) = loader {
        by_dest.insert(loader.to_path_buf(), loader.to_path_buf());
        if let Some(requested) = requested_loader {
            by_dest.insert(requested.to_path_buf(), loader.to_path_buf());
        }
    }
    for path in listed {
        // A listed loader alias maps to the checked file; anything else is itself.
        let source = match (loader, requested_loader) {
            (Some(loader), Some(requested)) if path == requested => loader.to_path_buf(),
            _ => path.clone(),
        };
        by_dest.entry(path.clone()).or_insert(source);
    }
    by_dest
        .into_iter()
        .map(|(dest, source)| Mount { source, dest })
        .collect()
}

fn discovery_argv(loader: &Path, executable: &Path) -> Vec<String> {
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
    for directory in std::iter::once(NIX_ROOT).chain(SYSTEM_DIRECTORIES) {
        args.extend([
            "--ro-bind-try".to_owned(),
            directory.to_owned(),
            directory.to_owned(),
        ]);
    }
    args.extend([
        "--ro-bind-try".to_owned(),
        "/etc/ld.so.cache".to_owned(),
        "/etc/ld.so.cache".to_owned(),
    ]);
    let executable = executable.to_string_lossy().into_owned();
    args.extend([
        "--ro-bind".to_owned(),
        executable.clone(),
        executable.clone(),
    ]);
    args.extend(["--chdir".to_owned(), "/tmp".to_owned()]);
    args.extend([
        loader.to_string_lossy().into_owned(),
        "--list".to_owned(),
        executable,
    ]);
    args
}

/// `interpreter` resolved, if it is a file the host may run: below a standard system
/// directory and root-owned, or a Nix store path, and not writable by group or
/// others along the whole path. Anything else (a loader inside a workspace, say) is
/// refused before it is executed.
fn trusted_loader(interpreter: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    let resolved = interpreter
        .canonicalize()
        .map_err(|error| format!("loader {}: {error}", interpreter.display()))?;
    let in_store = resolved.starts_with(NIX_STORE);
    let in_system = SYSTEM_DIRECTORIES
        .iter()
        .any(|directory| resolved.starts_with(directory));
    if !in_store && !in_system {
        return Err(refused(
            &resolved,
            "it is outside the system directories and the Nix store",
        ));
    }
    let mut checked = PathBuf::new();
    for component in resolved.components() {
        checked.push(component);
        // The store root itself is shared and sticky; only what is inside it is content-addressed.
        if in_store && Path::new(NIX_STORE).starts_with(&checked) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&checked)
            .map_err(|error| format!("loader {}: {error}", checked.display()))?;
        if metadata.mode() & 0o022 != 0 {
            return Err(refused(
                &resolved,
                &format!("{} is writable by group or others", checked.display()),
            ));
        }
        if !in_store && metadata.uid() != 0 {
            return Err(refused(
                &resolved,
                &format!("{} is not owned by root", checked.display()),
            ));
        }
    }
    Ok(resolved)
}

fn refused(loader: &Path, reason: &str) -> String {
    format!(
        "{} is not a trusted loader ({reason}); refusing to run it to list libraries",
        loader.display()
    )
}

/// Runs `command` in its own process group, killing and reaping the whole group
/// when it exceeds `timeout` or writes more than `cap` bytes, and returns what it
/// wrote to stdout. A command that ends successfully and stays within both limits
/// is the only success.
fn run_bounded(
    command: WrappedCommand,
    timeout: Duration,
    cap: usize,
    cancel: &AtomicBool,
) -> Result<String, String> {
    struct Prepared(WrappedCommand);
    impl Sandbox for Prepared {
        fn wrap(&self, _: CommandSpec) -> Result<WrappedCommand, SandboxError> {
            Ok(self.0.clone())
        }
    }
    let mut session = StdioSession::spawn(
        &Prepared(command),
        CommandSpec {
            program: String::new(),
            args: Vec::new(),
            cwd: None,
            extra_env: Vec::new(),
        },
    )
    .map_err(|error| error.to_string())?;
    drop(session.take_stdin());
    let (Some(mut stdout), Some(mut stderr)) = (session.take_stdout(), session.take_stderr())
    else {
        return Err("output pipes are unavailable".to_owned());
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let read = (&mut stdout).take(cap as u64 + 1).read_to_end(&mut output);
        let _ = sender.send(read.map(|_| output));
    });
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
    });
    let started = std::time::Instant::now();
    let output = loop {
        match receiver.recv_timeout(CANCEL_POLL) {
            Ok(Ok(output)) if output.len() <= cap => break output,
            Ok(Ok(_)) => return Err(format!("more than {cap} bytes of output")),
            Ok(Err(error)) => return Err(error.to_string()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("the output reader ended unexpectedly".to_owned());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Returning drops the session, which kills and reaps the process group.
                if cancel.load(Ordering::SeqCst) {
                    return Err("cancelled".to_owned());
                }
                if started.elapsed() >= timeout {
                    return Err(format!("no result within {} ms", timeout.as_millis()));
                }
            }
        }
    };
    // Output ended; the process must end too. The group is killed on drop either way.
    let exit_deadline = std::time::Instant::now() + EXIT_GRACE;
    let status = loop {
        match session.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= exit_deadline => {
                return Err("it closed its output but kept running".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(error.to_string()),
        }
    };
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
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
            executable: PathBuf::from("/opt/evorch"),
            mounts: plan_mounts(
                Path::new("/opt/evorch"),
                None,
                None,
                &[PathBuf::from("/lib/libc.so.6")],
            ),
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
                ("/lib/libc.so.6", "/lib/libc.so.6"),
                ("/opt/evorch", "/opt/evorch")
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
            executable: PathBuf::from("/opt/evorch"),
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
    fn fake_elf_with_interpreter(path: &Path, interpreter: &Path) {
        let interpreter = format!("{}\0", interpreter.display());
        let mut bytes = vec![0u8; 64 + 56];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        bytes[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        bytes[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&3u32.to_le_bytes());
        bytes[64 + 8..64 + 16].copy_from_slice(&120u64.to_le_bytes());
        bytes[64 + 32..64 + 40].copy_from_slice(&(interpreter.len() as u64).to_le_bytes());
        bytes.extend_from_slice(interpreter.as_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_loader_outside_the_system_is_never_executed() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let sentinel = directory.path().join("ran-on-the-host");
        let loader = directory.path().join("evil-loader");
        std::fs::write(
            &loader,
            format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        )
        .unwrap();
        std::fs::set_permissions(&loader, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = directory.path().join("worker");
        fake_elf_with_interpreter(&executable, &loader);

        let error = MinimalSandbox::detect(&executable, &[])
            .unwrap_err()
            .to_string();

        assert!(error.contains("not a trusted loader"), "{error}");
        assert!(!sentinel.exists(), "the loader ran on the host");
    }

    fn sh(script: &str) -> WrappedCommand {
        WrappedCommand {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            inherit_env: true,
            env: Vec::new(),
        }
    }

    fn gone(pid: i32) -> bool {
        rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap()).is_err()
    }

    #[test]
    fn a_listing_that_never_ends_is_killed_and_reaped_at_the_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 600", pid_file.display());

        let error = run_bounded(
            sh(&script),
            Duration::from_millis(200),
            1024,
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert!(error.contains("no result within"), "{error}");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid), "listing process {pid} survived");
    }

    #[test]
    fn a_listing_that_floods_its_output_is_stopped_at_the_cap() {
        let error = run_bounded(
            sh("yes x"),
            Duration::from_secs(60),
            1024,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("more than 1024 bytes"), "{error}");
    }

    #[test]
    fn a_listing_that_closes_its_output_but_keeps_running_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let script = format!(
            "echo $$ > {}; echo done; exec >&- 2>&-; exec sleep 600",
            pid_file.display()
        );
        let error = run_bounded(
            sh(&script),
            Duration::from_secs(60),
            1024,
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("kept running"), "{error}");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid), "listing process {pid} survived");
    }

    #[test]
    fn cancelling_a_listing_in_progress_kills_and_reaps_it() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("pid");
        let fifo = directory.path().join("started");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let script = format!(
            "echo $$ > {}; echo go > {}; exec sleep 600",
            pid_file.display(),
            fifo.display()
        );
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let listing = {
            let cancel = std::sync::Arc::clone(&cancel);
            std::thread::spawn(move || {
                run_bounded(sh(&script), Duration::from_secs(600), 1024, &cancel)
            })
        };
        // The listing is running once it has written to the FIFO.
        std::fs::read_to_string(&fifo).unwrap();

        cancel.store(true, Ordering::SeqCst);

        assert_eq!(listing.join().unwrap().unwrap_err(), "cancelled");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid), "listing process {pid} survived the cancellation");
    }

    #[test]
    fn the_loader_is_also_shown_at_the_path_the_elf_asks_for() {
        // A merged-usr system: the ELF asks for /lib64/ld-linux…, an alias of the file
        // that was checked and that the loader reports at its own path.
        let mounts = plan_mounts(
            Path::new("/opt/evorch"),
            Some(Path::new("/lib64/ld-linux-x86-64.so.2")),
            Some(Path::new("/usr/lib/ld-linux-x86-64.so.2")),
            &[
                PathBuf::from("/lib64/ld-linux-x86-64.so.2"),
                PathBuf::from("/usr/lib/libc.so.6"),
            ],
        );
        let of = |dest: &str| {
            mounts
                .iter()
                .find(|mount| mount.dest == Path::new(dest))
                .map(|mount| mount.source.clone())
        };
        assert_eq!(
            of("/lib64/ld-linux-x86-64.so.2"),
            Some(PathBuf::from("/usr/lib/ld-linux-x86-64.so.2"))
        );
        assert_eq!(
            of("/usr/lib/ld-linux-x86-64.so.2"),
            Some(PathBuf::from("/usr/lib/ld-linux-x86-64.so.2"))
        );
        assert_eq!(
            of("/usr/lib/libc.so.6"),
            Some(PathBuf::from("/usr/lib/libc.so.6"))
        );
        assert_eq!(of("/opt/evorch"), Some(PathBuf::from("/opt/evorch")));
        assert_eq!(mounts.len(), 4, "{mounts:?}");
    }

    #[test]
    fn a_successful_listing_returns_its_output() {
        assert_eq!(
            run_bounded(
                sh("echo listing"),
                Duration::from_secs(60),
                1024,
                &AtomicBool::new(false)
            )
            .unwrap(),
            "listing\n"
        );
    }
}
