//! Private helper-process backend. Both launch modes share authentication and reap.
use crate::{image::GuardImage, wire::LaunchMode};
use std::{
    io,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
};

#[derive(Debug)]
pub(crate) struct HelperProcess(Child);
impl HelperProcess {
    pub(crate) fn spawn(
        image: &GuardImage,
        mode: LaunchMode,
        name: &str,
        nonce: &str,
        stdio: [Stdio; 3],
    ) -> io::Result<Self> {
        let (path, flag) = match mode {
            LaunchMode::Direct => (image.inherited_exec_path(), "--launch"),
            LaunchMode::Pty => (image.pty_exec_path(), "--launch-pty"),
        };
        let [dev, ino] = image.identity_args();
        let [stdin, stdout, stderr] = stdio;
        Command::new(path)
            .args([
                flag,
                &dev,
                &ino,
                &std::process::id().to_string(),
                &unsafe { libc::getuid() }.to_string(),
                name,
                nonce,
            ])
            // Do not use portable-pty's spawn_command: even env_clear there
            // adds SHELL. No loader/target environment reaches this helper.
            .env_clear()
            .current_dir("/")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .map(Self)
    }
    pub(crate) fn id(&self) -> u32 {
        self.0.id()
    }
    pub(crate) fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
    pub(crate) fn wait(&mut self) -> io::Result<bool> {
        self.0.wait().map(|status| status.success())
    }
    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.0.stdin.take()
    }
    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.0.stdout.take()
    }
    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.0.stderr.take()
    }
}
