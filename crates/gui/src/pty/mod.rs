//! Terminal PTY session adapter (portable-pty).

use std::io::{Read, Write};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// PTYセッションの操作で発生するエラーです。
#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("PTY spawn failed: {0}")]
    Spawn(String),
    #[error("PTY I/O failed: {0}")]
    Io(String),
    #[error("PTY resize failed: {0}")]
    Resize(String),
    #[error("PTY session is closed")]
    Closed,
}

/// portable-ptyプロセスと、その入出力を管理するセッションです。
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    reader: Option<JoinHandle<()>>,
}

impl PtySession {
    /// PTYを開き、指定されたコマンドを起動します。
    pub fn spawn(
        command: CommandBuilder,
        rows: u16,
        cols: u16,
        on_output: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, TerminalError> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Spawn(error.to_string()))?;
        let child = pair
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
        let master = pair.master;
        let (output_tx, output_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        if output_tx.send(buffer[..length].to_vec()).is_err() {
                            break;
                        }
                        if let Some(callback) = &on_output {
                            callback();
                        }
                    }
                }
            }
        });

        Ok(Self {
            master,
            writer,
            child,
            output_rx,
            reader: Some(reader),
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

    /// 子プロセスを終了し、reader threadの終了を待ちます。
    pub fn kill(&mut self) -> Result<(), TerminalError> {
        let result = self
            .child
            .kill()
            .map_err(|error| TerminalError::Io(error.to_string()));
        self.join_reader();
        result
    }

    fn join_reader(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        self.join_reader();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use portable_pty::CommandBuilder;

    use super::PtySession;

    fn echo_session() -> PtySession {
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "read line; printf '%s\\n' \"$line\""]);
        PtySession::spawn(command, 24, 80, None).expect("echo shell must spawn")
    }

    fn wait_for_output(session: &mut PtySession, expected: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            output.extend(session.drain_output());
            if output
                .windows(expected.len())
                .any(|window| window == expected)
            {
                return output;
            }
            thread::sleep(Duration::from_millis(10));
        }
        output
    }

    #[test]
    fn pty_echo_roundtrip() {
        // Given: a real cat process attached to a PTY
        let mut session = echo_session();

        // When: input is written to the session
        session
            .write(b"hello from pty\n")
            .expect("PTY write must succeed");
        let output = wait_for_output(&mut session, b"hello from pty\r\n");

        // Then: the PTY returns the echoed line
        assert!(
            output
                .windows(16)
                .any(|window| window == b"hello from pty\r\n")
        );
    }

    #[test]
    fn pty_resize_succeeds() {
        // Given: a real cat process attached to a PTY
        let mut session = echo_session();

        // When: the terminal dimensions change
        let result = session.resize(40, 120);

        // Then: portable-pty accepts the resize
        assert!(result.is_ok());
    }

    #[test]
    fn pty_kill_terminates_reader() {
        // Given: a process that remains alive until explicitly killed
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let mut session = PtySession::spawn(command, 24, 80, None).expect("shell must spawn");

        // When: the process is killed
        session.kill().expect("child kill must succeed");

        // Then: dropping the session can join the reader without waiting for the shell
    }

    #[test]
    fn pty_drop_terminates_child() {
        // Given: a process that would otherwise outlive the test
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let on_output = Arc::new(|| {});
        let started = Instant::now();
        let session =
            PtySession::spawn(command, 24, 80, Some(on_output)).expect("shell must spawn");

        // When: the session is dropped
        drop(session);

        // Then: Drop returns promptly after terminating the child
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
