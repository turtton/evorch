//! Terminal PTY session adapter (portable-pty).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// PTY の出力到着・子プロセス終了を UI へ知らせるコールバックです。
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// 端末の初期 cwd を決めます。プロジェクトルートを優先し、`/` は使わずホームへ倒します。
pub fn resolve_terminal_cwd(primary: Option<&Path>, home: &Path) -> PathBuf {
    primary
        .filter(|path| *path != Path::new("/"))
        .map_or_else(|| home.to_path_buf(), Path::to_path_buf)
}

/// PTYセッションの操作で発生するエラーです。
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("PTY spawn failed: {0}")]
    Spawn(String),
    #[error("PTY I/O failed: {0}")]
    Io(String),
    #[error("PTY resize failed: {0}")]
    Resize(String),
}

/// 端末セッション用のプロセスを PTY 上に起動します。
pub trait TerminalSpawner: Send + Sync {
    fn spawn(
        &self,
        cwd: &Path,
        rows: u16,
        cols: u16,
        wake: Wake,
    ) -> Result<PtySession, TerminalError>;
}

/// ユーザーのシェル (`$SHELL`、未設定なら `/bin/sh`) を起動する spawner です。
#[derive(Debug, Clone)]
pub struct ShellSpawner {
    program: String,
    args: Vec<String>,
}

impl ShellSpawner {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }

    pub fn from_env() -> Self {
        let program = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_owned());
        Self::new(program, Vec::new())
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    fn command(&self, cwd: &Path) -> CommandBuilder {
        let mut command = CommandBuilder::new(&self.program);
        command.args(&self.args);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "evorch");
        command
    }
}

impl TerminalSpawner for ShellSpawner {
    fn spawn(
        &self,
        cwd: &Path,
        rows: u16,
        cols: u16,
        wake: Wake,
    ) -> Result<PtySession, TerminalError> {
        PtySession::spawn(self.command(cwd), rows, cols, Some(wake))
    }
}

/// portable-ptyプロセスと、その入出力を管理するセッションです。
///
/// 出力の読み取りと子プロセスの終了待ちはそれぞれ専用スレッドで行います。
/// 背景ジョブが PTY を握り続けても UI が待たされないよう、Drop ではスレッドを join しません。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    exit_code: Arc<OnceLock<u32>>,
}

impl PtySession {
    /// PTYを開き、指定されたコマンドを起動します。
    pub fn spawn(
        command: CommandBuilder,
        rows: u16,
        cols: u16,
        on_output: Option<Wake>,
    ) -> Result<Self, TerminalError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        drop(pair.slave);
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        let killer = child.clone_killer();
        let wake = on_output.unwrap_or_else(|| Arc::new(|| {}));
        let (output_tx, output_rx) = mpsc::channel();
        let reader_wake = Arc::clone(&wake);
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        if output_tx.send(buffer[..length].to_vec()).is_err() {
                            break;
                        }
                        reader_wake();
                    }
                }
            }
        });
        let exit_code = Arc::new(OnceLock::new());
        let exit = Arc::clone(&exit_code);
        thread::spawn(move || {
            let code = child.wait().map_or(1, |status| status.exit_code());
            let _ = exit.set(code);
            wake();
        });

        Ok(Self {
            master: pair.master,
            writer,
            killer,
            output_rx,
            exit_code,
        })
    }

    /// PTYへ入力バイト列を書き込みます。
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.writer
            .write_all(bytes)
            .and_then(|()| self.writer.flush())
            .map_err(|error| TerminalError::Io(error.to_string()))
    }

    /// PTYの行数と列数を変更します。
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), TerminalError> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Resize(error.to_string()))
    }

    /// reader threadから受信済みの出力を非ブロッキングで連結して返します。
    pub fn drain_output(&mut self) -> Vec<u8> {
        self.output_rx.try_iter().flatten().collect()
    }

    /// 子プロセスが終了していればその終了コードを返します。
    pub fn exit_code(&self) -> Option<u32> {
        self.exit_code.get().copied()
    }

    /// 子プロセスへ終了を要求します。終了は [`Self::exit_code`] で観測できます。
    pub fn kill(&mut self) -> Result<(), TerminalError> {
        self.killer
            .kill()
            .map_err(|error| TerminalError::Io(error.to_string()))
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.exit_code().is_none() {
            let _ = self.killer.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, mpsc};

    use portable_pty::CommandBuilder;

    use super::{PtySession, ShellSpawner, TerminalSpawner, resolve_terminal_cwd};

    #[test]
    fn terminal_cwd_uses_primary_or_home_and_never_root() {
        // Given: a primary project and home directory
        let primary = Path::new("/projects/primary");
        let home = Path::new("/home/tester");

        // When/Then: primary wins, a root primary and no primary fall back to home
        assert_eq!(resolve_terminal_cwd(Some(primary), home), primary);
        assert_eq!(resolve_terminal_cwd(Some(Path::new("/")), home), home);
        assert_eq!(resolve_terminal_cwd(None, home), home);
    }

    /// Spawns a command whose output and exit are signalled on the returned channel.
    fn spawn_signalled(command: CommandBuilder) -> (PtySession, mpsc::Receiver<()>) {
        let (tx, rx) = mpsc::channel();
        let wake = Arc::new(move || {
            let _ = tx.send(());
        });
        let session = PtySession::spawn(command, 24, 80, Some(wake)).expect("PTY must spawn");
        (session, rx)
    }

    fn read_until(session: &mut PtySession, wake: &mpsc::Receiver<()>, expected: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        while !output
            .windows(expected.len())
            .any(|window| window == expected)
        {
            wake.recv().expect("PTY must report output");
            output.extend(session.drain_output());
        }
        output
    }

    fn wait_exit(session: &PtySession, wake: &mpsc::Receiver<()>) -> u32 {
        loop {
            if let Some(code) = session.exit_code() {
                return code;
            }
            wake.recv().expect("PTY must report exit");
        }
    }

    #[test]
    fn shell_spawner_starts_in_cwd_with_terminal_env() {
        // Given: a spawner for /bin/sh that prints its cwd and TERM
        let directory = tempfile::tempdir().expect("temporary directory must be created");
        let spawner = ShellSpawner::new(
            "/bin/sh",
            vec!["-c".into(), "printf '%s|%s\\n' \"$PWD\" \"$TERM\"".into()],
        );
        let (tx, wake) = mpsc::channel();
        let mut session = spawner
            .spawn(
                directory.path(),
                24,
                80,
                Arc::new(move || {
                    let _ = tx.send(());
                }),
            )
            .expect("shell must spawn");

        // When: the shell reports its environment
        let expected = format!("{}|xterm-256color", directory.path().display());
        let output = read_until(&mut session, &wake, expected.as_bytes());

        // Then: the session runs in the requested directory as an xterm-256color terminal
        assert!(String::from_utf8_lossy(&output).contains(&expected));
    }

    #[test]
    fn pty_echo_roundtrip() {
        // Given: a shell that echoes one line read from the PTY
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "read line; printf 'got:%s\\n' \"$line\""]);
        let (mut session, wake) = spawn_signalled(command);

        // When: input is written to the session
        session
            .write(b"hello from pty\n")
            .expect("PTY write must succeed");

        // Then: the PTY returns the processed line
        read_until(&mut session, &wake, b"got:hello from pty");
    }

    #[test]
    fn pty_resize_succeeds() {
        // Given: a process attached to a PTY
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let (mut session, _wake) = spawn_signalled(command);

        // When: the terminal dimensions change
        session.resize(40, 120).expect("resize must succeed");

        // Then: portable-pty accepts the resize
        let size = session
            .master
            .get_size()
            .expect("PTY size must be readable");
        assert_eq!((size.rows, size.cols), (40, 120));
    }

    #[test]
    fn exit_code_is_reported_after_the_child_exits() {
        // Given: a command that exits with a specific status
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "exit 3"]);
        let (session, wake) = spawn_signalled(command);

        // When/Then: the waiter thread records the status and wakes the UI
        assert_eq!(wait_exit(&session, &wake), 3);
    }

    #[test]
    fn kill_terminates_the_child() {
        // Given: a process that remains alive until explicitly killed
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let (mut session, wake) = spawn_signalled(command);

        // When: the process is killed
        session.kill().expect("child kill must succeed");

        // Then: the exit is observed without waiting for the sleep
        wait_exit(&session, &wake);
    }

    #[test]
    fn drop_does_not_wait_for_background_jobs_holding_the_pty() {
        // Given: a shell whose background job keeps the PTY slave open
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30 & echo started; wait"]);
        let (mut session, wake) = spawn_signalled(command);
        read_until(&mut session, &wake, b"started");

        // When/Then: dropping the session returns instead of joining the blocked reader
        drop(session);
    }
}
