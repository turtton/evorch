//! Starting the JS worker process (ADR 0039): inside the minimal bubblewrap
//! sandbox of [`sandbox::MinimalSandbox`] (its own executable and libraries, a
//! private tmpfs, no network, no workspace, no credentials, and only
//! `LD_LIBRARY_PATH` from the environment). Failing to build that sandbox fails the call;
//! there is no fallback to running the worker unsandboxed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use sandbox::{CommandSpec, MinimalSandbox, Sandbox, StdioSession};
use tokio::io::BufReader;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::time::Instant;

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
    cancel_confirmation: Duration,
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
    /// A caller-supplied preparation step, to test how starting a worker is cancelled.
    #[cfg(test)]
    Preparing(Arc<PrepareFn>),
}

#[cfg(test)]
type PrepareFn = dyn Fn(&AtomicBool) -> Result<Prepared, String> + Send + Sync;

/// What it takes to start the worker, once it is known where it is and what it may see.
struct Prepared {
    sandbox: Arc<dyn Sandbox>,
    spec: CommandSpec,
    keep_alive: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

/// A sandbox and the command to run through it (what a test preparation yields).
#[cfg(test)]
pub(super) struct Sandboxed {
    pub sandbox: Arc<dyn Sandbox>,
    pub spec: CommandSpec,
}

/// Why no worker was started.
#[derive(Debug)]
pub enum LaunchError {
    /// The deadline passed first. Whatever was being prepared has been stopped.
    NotReady,
    Failed(String),
}

/// Longest wait for a cancelled preparation to confirm it has stopped.
const CANCEL_CONFIRMATION: Duration = Duration::from_secs(5);
const UNCONFIRMED_START: &str = "cancelling the start of the JS worker was not confirmed in time";

/// Set when the blocking preparation has ended, whatever the way.
#[derive(Default)]
struct Finished {
    done: Mutex<bool>,
    changed: Condvar,
}

impl Finished {
    fn signal(&self) {
        *self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }

    /// Whether the preparation ended within `limit`.
    fn wait(&self, limit: Duration) -> bool {
        let done = self
            .done
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (done, _) = self
            .changed
            .wait_timeout_while(done, limit, |done| !*done)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *done
    }
}

struct SignalOnDrop(Arc<Finished>);

impl Drop for SignalOnDrop {
    fn drop(&mut self) {
        self.0.signal();
    }
}

/// Stops a preparation that is still running when the launching future is dropped
/// (the tool call was cancelled): cancels it and waits, briefly and synchronously,
/// until it has killed what it started. If that cannot be confirmed the host is told,
/// so the run stops rather than carrying on with a process of unknown fate.
struct Preparation {
    cancel: Arc<AtomicBool>,
    finished: Arc<Finished>,
    on_unconfirmed: Arc<dyn Fn(String) + Send + Sync>,
    confirmation: Duration,
    armed: bool,
}

impl Drop for Preparation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancel.store(true, Ordering::SeqCst);
        if !self.finished.wait(self.confirmation) {
            (self.on_unconfirmed)(UNCONFIRMED_START.to_owned());
        }
    }
}

impl WorkerLauncher {
    #[cfg(test)]
    pub(super) fn preparing(
        prepare: impl Fn(&AtomicBool) -> Result<Sandboxed, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            mode: Mode::Preparing(Arc::new(move |cancel| {
                prepare(cancel).map(|sandboxed| Prepared {
                    sandbox: sandboxed.sandbox,
                    spec: sandboxed.spec,
                    keep_alive: Vec::new(),
                })
            })),
            cancel_confirmation: CANCEL_CONFIRMATION,
        }
    }

    pub const fn production() -> Self {
        Self {
            mode: Mode::Production,
            cancel_confirmation: CANCEL_CONFIRMATION,
        }
    }

    /// How long a cancelled start may take to confirm it stopped what it started.
    #[cfg(test)]
    pub(super) const fn with_cancel_confirmation(mut self, limit: Duration) -> Self {
        self.cancel_confirmation = limit;
        self
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
            cancel_confirmation: CANCEL_CONFIRMATION,
        }
    }

    /// Starts a worker, or gives up at `deadline`. Preparation (locating the worker,
    /// listing its libraries) runs on a blocking thread that is told to stop when
    /// the deadline passes or this future is dropped; it kills what it started and
    /// never starts the worker afterwards. When this returns an error, nothing
    /// started is left, unless `on_unconfirmed` was called.
    ///
    /// # Errors
    /// [`LaunchError::NotReady`] at the deadline, otherwise a message naming what
    /// is missing or what the sandbox refused.
    pub async fn launch(
        &self,
        deadline: Instant,
        on_unconfirmed: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<WorkerConnection, LaunchError> {
        let mode = self.mode.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(Finished::default());
        let mut preparation = Preparation {
            cancel: Arc::clone(&cancel),
            finished: Arc::clone(&finished),
            on_unconfirmed: Arc::clone(&on_unconfirmed),
            confirmation: self.cancel_confirmation,
            armed: true,
        };
        let task = tokio::task::spawn_blocking(move || {
            let _signal = SignalOnDrop(finished);
            spawn(&mode, &cancel)
        });
        tokio::pin!(task);
        let outcome = tokio::select! {
            joined = &mut task => Some(joined),
            () = tokio::time::sleep_until(deadline) => None,
        };
        let joined = if let Some(joined) = outcome {
            joined
        } else {
            preparation.cancel.store(true, Ordering::SeqCst);
            // The preparation polls the flag; wait for it to confirm, off the runtime thread's clock.
            match tokio::time::timeout(self.cancel_confirmation, &mut task).await {
                Ok(joined) => drop(joined),
                Err(_) => on_unconfirmed(UNCONFIRMED_START.to_owned()),
            }
            preparation.armed = false;
            return Err(LaunchError::NotReady);
        };
        preparation.armed = false;
        let spawned = joined
            .map_err(|error| LaunchError::Failed(format!("worker launch task failed: {error}")))?
            .map_err(LaunchError::Failed)?;
        spawned.into_connection().map_err(LaunchError::Failed)
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
                    #[cfg(test)]
                    Mode::Preparing(_) => "preparing",
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
    writer: Option<ChildStdin>,
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

    /// Hands the stdin writer to a task that sends the host's messages.
    pub fn take_writer(&mut self) -> Option<ChildStdin> {
        self.writer.take()
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
            writer: Some(writer),
            stderr_tail,
            stderr_task: Some(stderr_task),
            session: Some(self.session),
            _keep_alive: self.keep_alive,
        })
    }
}

/// Prepares and starts the worker. `cancel` is checked after every step that
/// takes time: once it is set no worker is started, and one already started is
/// killed and reaped before this returns.
fn spawn(mode: &Mode, cancel: &AtomicBool) -> Result<Spawned, String> {
    let cancelled = || "the start of the JS worker was cancelled".to_owned();
    let prepared = match mode {
        #[cfg(test)]
        Mode::Preparing(prepare) => prepare(cancel)?,
        Mode::Injected {
            sandbox,
            program,
            args,
        } => Prepared {
            sandbox: Arc::clone(sandbox),
            spec: CommandSpec {
                program: program.to_string_lossy().into_owned(),
                args: args.clone(),
                cwd: None,
                extra_env: library_path(),
            },
            keep_alive: Vec::new(),
        },
        Mode::Production => {
            let program = locate_worker()?;
            let sandbox = MinimalSandbox::detect_cancellable(&program, &[LIBRARY_PATH_ENV], cancel)
                .map_err(|error| format!("bubblewrap is unavailable: {error}"))?;
            let spec = CommandSpec {
                program: sandbox.executable().to_string_lossy().into_owned(),
                args: vec![WORKER_SUBCOMMAND.to_owned()],
                cwd: None,
                extra_env: Vec::new(),
            };
            Prepared {
                sandbox: Arc::new(sandbox.clone()),
                spec,
                keep_alive: vec![Box::new(sandbox)],
            }
        }
    };
    if cancel.load(Ordering::SeqCst) {
        return Err(cancelled());
    }
    let spawned = start(
        prepared.sandbox.as_ref(),
        prepared.spec,
        prepared.keep_alive,
    )?;
    if cancel.load(Ordering::SeqCst) {
        // Dropping kills the process group and reaps it.
        drop(spawned);
        return Err(cancelled());
    }
    Ok(spawned)
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
        return resolve_worker(&path)
            .ok_or_else(|| format!("{WORKER_ENV}={} is not a file", path.display()));
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    let found = worker_beside(&current)?;
    resolve_worker(&found).ok_or_else(|| format!("{} is not a file", found.display()))
}

/// The worker file with symlinks and relative parts resolved, so the sandbox mounts
/// and runs one and the same path.
fn resolve_worker(path: &Path) -> Option<PathBuf> {
    path.canonicalize()
        .ok()
        .filter(|resolved| resolved.is_file())
}

/// The evorch binary that serves the worker for a process running as
/// `current`: `current` itself when it is `evorch`, otherwise the `evorch`
/// next to it. Other executables (the GUI, a test harness) cannot serve it.
fn worker_beside(current: &Path) -> Result<PathBuf, String> {
    let name = format!("evorch{}", std::env::consts::EXE_SUFFIX);
    if current
        .file_name()
        .is_some_and(|file| *file == *name.as_str())
        && let Some(path) = existing_file(current)
    {
        return Ok(path);
    }
    let sibling = current.with_file_name(&name);
    existing_file(&sibling).ok_or_else(|| {
        format!(
            "no evorch binary to serve the JS worker: {} is not evorch and {} does not exist; \
             put the evorch CLI binary (or a symlink to it) at that path when packaging, \
             or set {WORKER_ENV} to the evorch binary",
            current.display(),
            sibling.display()
        )
    })
}

fn existing_file(path: &Path) -> Option<PathBuf> {
    path.is_file().then(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn a_relative_or_symlinked_worker_path_is_resolved_to_the_real_file() {
        let directory = tempfile::tempdir().unwrap();
        let real = touch(directory.path(), "real-evorch");
        std::os::unix::fs::symlink(&real, directory.path().join("link")).unwrap();
        let through_dots = directory.path().join("././link");

        assert_eq!(
            resolve_worker(&through_dots),
            Some(real.canonicalize().unwrap())
        );
        assert_eq!(resolve_worker(&directory.path().join("absent")), None);
        assert_eq!(
            resolve_worker(directory.path()),
            None,
            "a directory is not a worker"
        );
    }

    #[test]
    fn evorch_serves_the_worker_itself() {
        let directory = tempfile::tempdir().unwrap();
        let evorch = touch(directory.path(), "evorch");
        touch(directory.path(), "evorch-gui");

        assert_eq!(worker_beside(&evorch).unwrap(), evorch);
    }

    #[test]
    fn gui_binary_uses_the_evorch_beside_it() {
        let directory = tempfile::tempdir().unwrap();
        let evorch = touch(directory.path(), "evorch");
        let gui = touch(directory.path(), "evorch-gui");

        assert_eq!(worker_beside(&gui).unwrap(), evorch);
    }

    /// How the Nix GUI package lays out `libexec/<package>/`: the GUI
    /// executable and a symlink to the `evorch` of another store path.
    #[test]
    fn gui_binary_uses_an_evorch_symlinked_beside_it() {
        let store = tempfile::tempdir().unwrap();
        let real = touch(store.path(), "evorch");
        let libexec = tempfile::tempdir().unwrap();
        let gui = touch(libexec.path(), "evorch-gui");
        let link = libexec.path().join(real.file_name().unwrap());
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let found = worker_beside(&gui).unwrap();

        assert_eq!(found, link);
        assert_eq!(
            resolve_worker(&found),
            Some(real.canonicalize().unwrap()),
            "the sandbox must mount and run the file the link points to"
        );
    }

    #[test]
    fn a_dangling_evorch_symlink_beside_the_gui_is_not_a_worker() {
        let directory = tempfile::tempdir().unwrap();
        let gui = touch(directory.path(), "evorch-gui");
        let link = directory
            .path()
            .join(format!("evorch{}", std::env::consts::EXE_SUFFIX));
        std::os::unix::fs::symlink(directory.path().join("gone"), link).unwrap();

        let error = worker_beside(&gui).unwrap_err();

        assert!(error.contains("no evorch binary"), "{error}");
    }

    #[test]
    fn gui_binary_without_evorch_beside_it_is_a_clear_error() {
        let directory = tempfile::tempdir().unwrap();
        let gui = touch(directory.path(), "evorch-gui");

        let error = worker_beside(&gui).unwrap_err();

        assert!(error.contains("no evorch binary"), "{error}");
        assert!(error.contains(WORKER_ENV), "{error}");
    }
}
