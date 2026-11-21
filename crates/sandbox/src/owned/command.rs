//! Typed target-only configuration. No raw Command or pre_exec conversion.
use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Stdio},
};

/// A non-PTY launch. Inherited closed stdio is normalized to /dev/null by
/// helper startup; explicit pipes/null/files retain their usual semantics.
///
/// Raw commands cannot cross the ownership boundary:
/// ```compile_fail
/// let mut raw = tokio::process::Command::new("true");
/// let _ = sandbox::owned::OwnedChild::spawn(&mut raw);
/// ```
#[derive(Debug)]
pub struct OwnedCommand {
    command: Command,
    inherit_env: bool,
    /// std replaces an argument holding a NUL with a placeholder and only fails
    /// at spawn; the owner API keeps the failure rather than run the placeholder.
    saw_nul: bool,
    #[cfg(target_os = "linux")]
    runtime: Option<process_guard::GuardRuntime>,
    stdio: [Option<Stdio>; 3],
}
impl OwnedCommand {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref();
        Self {
            command: Command::new(program),
            inherit_env: true,
            saw_nul: has_nul(program),
            #[cfg(target_os = "linux")]
            runtime: None,
            stdio: [None, None, None],
        }
    }
    /// Explicit library runtime takes precedence over installed/inherited policy.
    #[cfg(target_os = "linux")]
    pub fn guard_runtime(&mut self, runtime: &process_guard::GuardRuntime) -> &mut Self {
        self.runtime = Some(runtime.clone());
        self
    }
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.saw_nul |= has_nul(arg.as_ref());
        self.command.arg(arg);
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.saw_nul |= has_nul(key.as_ref()) || has_nul(value.as_ref());
        self.command.env(key, value);
        self
    }
    pub fn envs<I, K, V>(&mut self, env: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in env {
            self.env(key, value);
        }
        self
    }
    pub fn env_clear(&mut self) -> &mut Self {
        self.inherit_env = false;
        self.command.env_clear();
        self
    }
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.saw_nul |= has_nul(key.as_ref());
        self.command.env_remove(key);
        self
    }
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.saw_nul |= has_nul(dir.as_ref().as_os_str());
        self.command.current_dir(dir);
        self
    }
    pub fn stdin(&mut self, fd: impl Into<Stdio>) -> &mut Self {
        self.stdio[0] = Some(fd.into());
        self
    }
    pub fn stdout(&mut self, fd: impl Into<Stdio>) -> &mut Self {
        self.stdio[1] = Some(fd.into());
        self
    }
    pub fn stderr(&mut self, fd: impl Into<Stdio>) -> &mut Self {
        self.stdio[2] = Some(fd.into());
        self
    }
    /// The error std reports at spawn for a NUL in any argument, name or path.
    pub(super) fn check_nul(&self) -> std::io::Result<()> {
        if self.saw_nul {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "nul byte found in provided data",
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub(super) fn launch(&mut self) -> std::io::Result<process_guard::GuardChild> {
        use std::{collections::BTreeMap, os::unix::ffi::OsStrExt};
        self.check_nul()?;
        let mut env = if self.inherit_env {
            std::env::vars_os().collect::<BTreeMap<_, _>>()
        } else {
            BTreeMap::new()
        };
        for (key, value) in self.command.get_envs() {
            if let Some(value) = value {
                env.insert(key.to_owned(), value.to_owned());
            } else {
                env.remove(key);
            }
        }
        let cwd = self
            .command
            .get_current_dir()
            .map(Path::to_path_buf)
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)?;
        let cwd = if cwd.is_absolute() {
            cwd
        } else {
            std::env::current_dir()?.join(cwd)
        };
        let spec = process_guard::LaunchSpec {
            argv: std::iter::once(self.command.get_program())
                .chain(self.command.get_args())
                .map(|v| v.as_bytes().to_vec())
                .collect(),
            env: env
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
            cwd: cwd.as_os_str().as_bytes().to_vec(),
        };
        process_guard::GuardRuntime::for_library(self.runtime.as_ref())?.spawn(
            &spec,
            self.stdio
                .each_mut()
                .map(|fd| fd.take().unwrap_or_else(Stdio::inherit)),
        )
    }
    #[cfg(not(target_os = "linux"))]
    pub(super) fn raw(&mut self) -> std::io::Result<Command> {
        self.check_nul()?;
        let mut command = Command::new(self.command.get_program());
        command.args(self.command.get_args());
        if !self.inherit_env {
            command.env_clear();
        }
        for (key, value) in self.command.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        if let Some(cwd) = self.command.get_current_dir() {
            command.current_dir(cwd);
        }
        let [input, output, error] = self
            .stdio
            .each_mut()
            .map(|fd| fd.take().unwrap_or_else(Stdio::inherit));
        command.stdin(input).stdout(output).stderr(error);
        Ok(command)
    }
}

fn has_nul(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}
