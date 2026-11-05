//! Records the working tree a new thread starts from (ADR 0036 §2).
//!
//! One read-only pass of the git CLI over the project's repository, bounded by
//! [`CAPTURE_DEADLINE`] and [`MAX_THREAD_START_BYTES`]. Anything that cannot be
//! captured completely is recorded as not replayable instead of truncated.

use std::io::Read;
use std::path::Path;
use std::sync::Weak;
use std::time::{Duration, Instant};

use storage::thread_start::{
    MAX_THREAD_START_BYTES, NotReplayableReason, ThreadStartCapture, ThreadStartSnapshot,
    ThreadStartState, UntrackedEntry, UntrackedKind,
};

use crate::RunId;
use crate::git_read::{GitOutput, GitReadError, GitReader, STDERR_LIMIT};
use crate::runtime::Shared;

pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_secs(10);

/// Capture and persist the start state before the thread's first run touches the tree.
/// Never fails the run: storage errors are logged and the wait is bounded by the deadline.
pub(crate) async fn record(
    shared: &Weak<Shared>,
    thread_id: String,
    root_run_id: RunId,
    project_root: std::path::PathBuf,
) {
    let Some(handle) = shared
        .upgrade()
        .and_then(|shared| shared.run_store.get().map(|store| store.handle.clone()))
    else {
        return;
    };
    let captured_at_ns = crate::run_store::review_time_ns();
    let root = project_root.clone();
    let deadline = Instant::now() + CAPTURE_DEADLINE;
    let capture = tokio::time::timeout(
        CAPTURE_DEADLINE,
        tokio::task::spawn_blocking(move || capture(&root, deadline)),
    )
    .await
    .unwrap_or(Ok(ThreadStartCapture::NotReplayable(
        NotReplayableReason::TimedOut,
    )))
    .unwrap_or(ThreadStartCapture::NotReplayable(
        NotReplayableReason::CaptureFailed,
    ));
    let state = ThreadStartState {
        thread_id,
        root_run_id: root_run_id.to_string(),
        project_root: project_root.to_string_lossy().into_owned(),
        captured_at_ns,
        capture,
    };
    match tokio::task::spawn_blocking(move || handle.save_thread_start_state(state)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(run_id = %root_run_id, %error, "thread start state was not saved");
        }
        Err(error) => {
            tracing::warn!(run_id = %root_run_id, %error, "thread start state save panicked");
        }
    }
}

pub(crate) fn capture(project_root: &Path, deadline: Instant) -> ThreadStartCapture {
    capture_with(project_root, &GitReader::new(deadline))
}

fn capture_with(project_root: &Path, git: &GitReader) -> ThreadStartCapture {
    match snapshot(project_root, git) {
        Ok(snapshot) => ThreadStartCapture::Replayable(snapshot),
        Err(reason) => ThreadStartCapture::NotReplayable(reason),
    }
}

fn snapshot(
    project_root: &Path,
    git: &GitReader,
) -> Result<ThreadStartSnapshot, NotReplayableReason> {
    let located = run(
        git,
        project_root,
        &["rev-parse", "--show-toplevel", "--show-prefix"],
        STDERR_LIMIT,
    )?;
    if !located.status.success() {
        return Err(
            if String::from_utf8_lossy(&located.stderr).contains("not a git repository") {
                NotReplayableReason::NotGitRepository
            } else {
                NotReplayableReason::CaptureFailed
            },
        );
    }
    let located =
        String::from_utf8(located.stdout).map_err(|_| NotReplayableReason::CaptureFailed)?;
    let mut lines = located.split('\n');
    let toplevel = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or(NotReplayableReason::CaptureFailed)?;
    let repo_prefix = lines.next().unwrap_or_default().to_owned();
    let toplevel = Path::new(toplevel);

    let head = run(
        git,
        toplevel,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        STDERR_LIMIT,
    )?;
    if !head.status.success() {
        return Err(NotReplayableReason::NoCommit);
    }
    let head_commit = String::from_utf8(head.stdout)
        .map_err(|_| NotReplayableReason::CaptureFailed)?
        .trim()
        .to_owned();
    let head_ref = head_ref(toplevel, git)?;

    // Fixed flags keep the diff applicable whatever the user's diff config says.
    let diff = succeeded(run(
        git,
        toplevel,
        &[
            "diff",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--submodule=short",
            "HEAD",
            "--",
        ],
        MAX_THREAD_START_BYTES,
    )?)?;
    let listed = succeeded(run(
        git,
        toplevel,
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
        MAX_THREAD_START_BYTES,
    )?)?;

    let mut total = diff.len();
    let mut untracked = Vec::new();
    for path in listed.split(|byte| *byte == 0) {
        // Nested repositories are listed as directories; their content is not ours to record.
        if path.is_empty() || path.ends_with(b"/") {
            continue;
        }
        if Instant::now() >= git.deadline() {
            return Err(NotReplayableReason::TimedOut);
        }
        let path =
            String::from_utf8(path.to_vec()).map_err(|_| NotReplayableReason::CaptureFailed)?;
        let Some(entry) = read_untracked(toplevel, path, MAX_THREAD_START_BYTES - total)? else {
            continue;
        };
        total += entry.content.len();
        untracked.push(entry);
    }
    Ok(ThreadStartSnapshot {
        repo_prefix,
        head_commit,
        head_ref,
        diff,
        untracked,
    })
}

/// `git symbolic-ref` exits with 1 when `HEAD` is detached.
fn head_ref(toplevel: &Path, git: &GitReader) -> Result<Option<String>, NotReplayableReason> {
    let output = run(git, toplevel, &["symbolic-ref", "-q", "HEAD"], STDERR_LIMIT)?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    let name = String::from_utf8(succeeded(output)?)
        .map_err(|_| NotReplayableReason::CaptureFailed)?
        .trim()
        .to_owned();
    Ok(Some(name))
}

/// `None` for entries that are neither regular files nor symlinks, or vanished since listing.
fn read_untracked(
    toplevel: &Path,
    path: String,
    budget: usize,
) -> Result<Option<UntrackedEntry>, NotReplayableReason> {
    let full = toplevel.join(&path);
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(NotReplayableReason::CaptureFailed),
    };
    let (kind, executable, content) = if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(&full).map_err(|_| NotReplayableReason::CaptureFailed)?;
        (UntrackedKind::Symlink, false, path_bytes(&target))
    } else if metadata.is_file() {
        if metadata.len() > budget as u64 {
            return Err(NotReplayableReason::TooLarge);
        }
        let mut content = Vec::new();
        std::fs::File::open(&full)
            .and_then(|file| file.take(budget as u64 + 1).read_to_end(&mut content))
            .map_err(|_| NotReplayableReason::CaptureFailed)?;
        (UntrackedKind::File, is_executable(&metadata), content)
    } else {
        return Ok(None);
    };
    if content.len() > budget {
        return Err(NotReplayableReason::TooLarge);
    }
    Ok(Some(UntrackedEntry {
        path,
        kind,
        executable,
        content,
    }))
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

fn succeeded(output: GitOutput) -> Result<Vec<u8>, NotReplayableReason> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(NotReplayableReason::CaptureFailed)
    }
}

/// One bounded git read, its failures spelled as the reasons a start state records.
fn run(
    git: &GitReader,
    dir: &Path,
    arguments: &[&str],
    limit: usize,
) -> Result<GitOutput, NotReplayableReason> {
    git.run(dir, arguments, limit).map_err(|error| match error {
        GitReadError::TimedOut => NotReplayableReason::TimedOut,
        GitReadError::TooLarge => NotReplayableReason::TooLarge,
        GitReadError::Failed => NotReplayableReason::CaptureFailed,
    })
}

#[cfg(test)]
pub(crate) mod tests;
