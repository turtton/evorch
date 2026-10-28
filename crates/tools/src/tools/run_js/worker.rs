//! Starting the JS worker process (ADR 0039): inside bubblewrap with an empty
//! directory as its only writable path, no network, no workspace, no credentials
//! and no inherited environment. Failing to build that sandbox fails the call;
//! there is no fallback to running the worker unsandboxed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sandbox::{BwrapConfig, BwrapSandbox, CommandSpec, Sandbox, StdioSession};
use tokio::io::BufReader;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};

/// Environment variable naming the evorch binary that serves `js-worker`.
pub const WORKER_ENV: &str = "EVORCH_JS_WORKER";
/// The subcommand of the evorch binary that runs the worker.
pub const WORKER_SUBCOMMAND: &str = "js-worker";
const STDERR_TAIL_BYTES: usize = 4096;
/// Longest wait for a dead worker's stderr to drain.
const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_secs(1);
/// The dynamic loader's search path is the only variable the worker inherits: a
/// development build of evorch finds its C++ runtime through it.
const LIBRARY_PATH_ENV: &str = "LD_LIBRARY_PATH";

/// How the worker process is started. Cheap to build; nothing is detected or
/// created until [`WorkerLauncher::launch`].
#[derive(Clone)]
pub struct WorkerLauncher {
    mode: Mode,
}

#[derive(Clone)]
enum Mode {
    /// bubblewrap, with the worker binary found at launch time.
    Production,
    /// A caller-supplied sandbox and command, for tests and fakes.
    Injected {
        sandbox: Arc<dyn Sandbox>,
        program: PathBuf,
        args: Vec<String>,
    },
}

impl WorkerLauncher {
    pub const fn production() -> Self {
        Self {
            mode: Mode::Production,
        }
    }

    /// Runs `program args` through `sandbox` instead of the production setup.
    /// A [`sandbox::DirectSandbox`] gives no isolation; tests only.
    pub fn injected(sandbox: Arc<dyn Sandbox>, program: PathBuf, args: Vec<String>) -> Self {
        Self {
            mode: Mode::Injected {
                sandbox,
                program,
                args,
            },
        }
    }

    /// Starts a worker.
    ///
    /// # Errors
    /// A message naming what is missing or what the sandbox refused.
    pub async fn launch(&self) -> Result<WorkerConnection, String> {
        let mode = self.mode.clone();
        let spawned = tokio::task::spawn_blocking(move || spawn(&mode))
            .await
            .map_err(|error| format!("worker launch task failed: {error}"))??;
        spawned.into_connection()
    }
}

impl std::fmt::Debug for WorkerLauncher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkerLauncher")
            .field(
                "mode",
                &match self.mode {
                    Mode::Production => "production",
                    Mode::Injected { .. } => "injected",
                },
            )
            .finish()
    }
}

/// A running worker. Dropping it kills the whole process group and reaps it
/// before returning, so a finished, timed-out or cancelled call never leaves
/// the worker behind.
pub struct WorkerConnection {
    reader: Option<BufReader<ChildStdout>>,
    pub writer: ChildStdin,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    session: Option<StdioSession>,
    _keep_alive: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

impl WorkerConnection {
    /// Waits (briefly) for the worker's stderr to end, so a worker that died at
    /// start has its message captured before it is reported.
    pub async fn settle_stderr(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            let _ = tokio::time::timeout(STDERR_SETTLE, task).await;
        }
    }

    /// Hands the stdout reader to a task that forwards the worker's messages.
    pub fn take_reader(&mut self) -> Option<BufReader<ChildStdout>> {
        self.reader.take()
    }

    /// The last bytes the worker wrote to stderr, for failure messages.
    pub fn stderr_tail(&self) -> String {
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&tail).trim().to_owned()
    }
}

impl Drop for WorkerConnection {
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        // Kills and reaps the process group; the pipes close with the fields.
        drop(self.session.take());
    }
}

/// A started process whose pipes are still blocking std handles.
struct Spawned {
    session: StdioSession,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    keep_alive: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

impl Spawned {
    fn into_connection(self) -> Result<WorkerConnection, String> {
        let describe = |error: std::io::Error| format!("worker pipes unavailable: {error}");
        let writer = ChildStdin::from_std(self.stdin).map_err(describe)?;
        let reader = BufReader::new(ChildStdout::from_std(self.stdout).map_err(describe)?);
        let mut stderr = ChildStderr::from_std(self.stderr).map_err(describe)?;
        let stderr_tail = Arc::<Mutex<Vec<u8>>>::default();
        let tail = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0u8; 1024];
            while let Ok(read) = stderr.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let mut tail = tail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                tail.extend_from_slice(&buffer[..read]);
                let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
                tail.drain(..excess);
            }
        });
        Ok(WorkerConnection {
            reader: Some(reader),
            writer,
            stderr_tail,
            stderr_task: Some(stderr_task),
            session: Some(self.session),
            _keep_alive: self.keep_alive,
        })
    }
}

fn spawn(mode: &Mode) -> Result<Spawned, String> {
    match mode {
        Mode::Injected {
            sandbox,
            program,
            args,
        } => start(
            sandbox.as_ref(),
            CommandSpec {
                program: program.to_string_lossy().into_owned(),
                args: args.clone(),
                cwd: None,
                extra_env: library_path(),
            },
            Vec::new(),
        ),
        Mode::Production => {
            let program = locate_worker()?;
            let directory = tempfile::Builder::new()
                .prefix("evorch-js-worker-")
                .tempdir()
                .map_err(|error| format!("cannot create the worker directory: {error}"))?;
            let config = BwrapConfig::new(directory.path().to_path_buf())
                .without_tool_caches()
                .isolate_processes()
                .ro_bind(program.clone());
            let sandbox = BwrapSandbox::detect(config)
                .map_err(|error| format!("bubblewrap is unavailable: {error}"))?;
            start(
                &sandbox,
                CommandSpec {
                    program: program.to_string_lossy().into_owned(),
                    args: vec![WORKER_SUBCOMMAND.to_owned()],
                    cwd: Some(directory.path().to_path_buf()),
                    extra_env: library_path(),
                },
                vec![Box::new(sandbox.clone()), Box::new(directory)],
            )
        }
    }
}

fn start(
    sandbox: &dyn Sandbox,
    spec: CommandSpec,
    keep_alive: Vec<Box<dyn std::any::Any + Send + Sync>>,
) -> Result<Spawned, String> {
    let mut session = StdioSession::spawn(sandbox, spec)
        .map_err(|error| format!("cannot start the JS worker: {error}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) = (
        session.take_stdin(),
        session.take_stdout(),
        session.take_stderr(),
    ) else {
        return Err("worker pipes are unavailable".to_owned());
    };
    Ok(Spawned {
        session,
        stdin,
        stdout,
        stderr,
        keep_alive,
    })
}

fn library_path() -> Vec<(String, String)> {
    std::env::var(LIBRARY_PATH_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| (LIBRARY_PATH_ENV.to_owned(), value))
        .into_iter()
        .collect()
}

fn locate_worker() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(WORKER_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        return existing_file(&path)
            .ok_or_else(|| format!("{WORKER_ENV}={} is not a file", path.display()));
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    if current
        .file_stem()
        .is_some_and(|stem| stem == "evorch" || stem == "evorch-gui")
        && let Some(path) = existing_file(&current)
    {
        return Ok(path);
    }
    let sibling = current.with_file_name("evorch");
    existing_file(&sibling).ok_or_else(|| {
        format!(
            "no evorch binary to serve the JS worker: {} is not evorch and {} does not exist; \
             set {WORKER_ENV} to the evorch binary",
            current.display(),
            sibling.display()
        )
    })
}

fn existing_file(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}
