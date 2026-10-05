//! 編集成功後に、設定済みの外部 comment-checker を sandbox 内で実行する。

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use config::CommentCheckerConfig;
use sandbox::{CommandSpec, Sandbox};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, watch};

use crate::ToolResult;

pub const DEFAULT_PROMPT: &str = "Review only comments added or changed by this edit. Avoid comments that merely restate the code. Preserve comments that explain rationale, external constraints, invariants, safety justification, required API documentation, license notices, or tool directives. {{comments}}";
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
// Leave room for the quote wrapper and artifact notice in the final tail preview,
// including when the edit's diff already exceeds both output ceilings.
const WARNING_PREVIEW_BYTES: usize = crate::output::PREVIEW_BYTES / 4;
const WARNING_PREVIEW_LINES: usize = crate::output::PREVIEW_LINES / 4;

#[derive(Debug, Clone)]
pub struct PostEditInput {
    pub tool_name: &'static str,
    pub file_path: PathBuf,
    pub content: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostEditOutcome {
    Pass,
    Warning {
        message: String,
    },
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    SpawnFailed,
    /// Signal termination has no exit code and is represented by -1.
    UnexpectedExitCode(i32),
    Timeout,
    Io,
    InputTooLarge,
    PreviousContentsUnavailable,
}

/// Advisory inspection only. The executor's wrapper authorizes the checker
/// subprocess separately; editing permission does not authorize process creation.
#[async_trait]
pub trait PostEditHook: Send + Sync {
    async fn check(&self, input: &PostEditInput) -> PostEditOutcome;

    /// Wait for cancellation cleanup before releasing the run's workspace.
    async fn drain(&self) {}

    /// Stateful hooks share a one-shot notification across Write and Edit.
    fn take_unavailable_notification(&self) -> bool {
        true
    }
}

/// Fixed CLI invocation; sandbox and execution policy are supplied by the caller.
/// The binary and its dependencies must be visible inside that sandbox.
pub struct CommentChecker {
    sandbox: Arc<dyn Sandbox>,
    program: PathBuf,
    timeout: Duration,
    prompt: String,
    unavailable_notified: AtomicBool,
    supervisors: watch::Sender<usize>,
}

impl CommentChecker {
    /// Convenience for tests without a project. Production must supply both the
    /// original project and the active worktree to `resolve_with_roots`.
    pub fn resolve(sandbox: Arc<dyn Sandbox>, config: &CommentCheckerConfig) -> Option<Self> {
        Self::resolve_with_roots(sandbox, config, &[])
    }

    pub fn resolve_with_roots(
        sandbox: Arc<dyn Sandbox>,
        config: &CommentCheckerConfig,
        forbidden_roots: &[PathBuf],
    ) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let Some(roots) = verified_roots(forbidden_roots) else {
            tracing::warn!("comment-checker disabled: cannot verify forbidden roots");
            return None;
        };
        let program = if config.binary.contains('/') {
            let program = executable_path(Path::new(&config.binary), &roots);
            if program.is_none() {
                tracing::warn!(binary = %config.binary, "comment-checker explicit path is unavailable, forbidden, or not executable");
            }
            program
        } else {
            resolve_in_path(&config.binary, std::env::var_os("PATH").as_deref(), &roots)
        }?;
        Some(Self {
            sandbox,
            program,
            timeout: Duration::from_millis(config.timeout_ms),
            prompt: config
                .prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_PROMPT.to_owned()),
            unavailable_notified: AtomicBool::new(false),
            supervisors: watch::channel(0).0,
        })
    }
}

fn normalized_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    Some(normalized)
}

fn verified_roots(roots: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut verified = Vec::with_capacity(roots.len() * 2);
    for root in roots {
        verified.push(normalized_absolute(root)?);
        verified.push(root.canonicalize().ok()?);
    }
    Some(verified)
}

fn executable_path(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let normalized = normalized_absolute(path)?;
    if roots
        .iter()
        .any(|root| path.starts_with(root) || normalized.starts_with(root))
    {
        return None;
    }
    // A candidate reached through an alias of a project directory is still
    // project-controlled, even if its final symlink points back outside it.
    for ancestor in path.ancestors().skip(1) {
        let resolved = ancestor.canonicalize().ok()?;
        if roots.iter().any(|root| resolved.starts_with(root)) {
            return None;
        }
    }
    let canonical = path.canonicalize().ok()?;
    if roots.iter().any(|root| canonical.starts_with(root)) || canonical.to_str().is_none() {
        return None;
    }
    let metadata = canonical.metadata().ok()?;
    (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(canonical)
}

fn resolve_in_path(
    binary: &str,
    path: Option<&std::ffi::OsStr>,
    roots: &[PathBuf],
) -> Option<PathBuf> {
    let mut parts = Path::new(binary).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
        return None;
    }
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| executable_path(&dir.join(binary), roots))
}

fn unavailable(reason: UnavailableReason, detail: impl ToString) -> PostEditOutcome {
    PostEditOutcome::Unavailable {
        reason,
        detail: detail.to_string(),
    }
}

/// Check the lower bound before tools duplicate their borrowed inputs. JSON
/// escaping and structural overhead are checked by the bounded writer below.
pub(crate) fn input_fits(path: &Path, parts: &[&str]) -> bool {
    parts
        .iter()
        .try_fold(path.as_os_str().len(), |size, part| {
            size.checked_add(part.len())
        })
        .is_some_and(|size| size <= MAX_INPUT_BYTES)
}

struct BoundedPayload(Vec<u8>);
impl io::Write for BoundedPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_INPUT_BYTES - self.0.len() {
            return Err(io::Error::other("checker JSON input exceeds 2 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn payload(input: &PostEditInput) -> Result<Vec<u8>, PostEditOutcome> {
    #[derive(Serialize)]
    struct ToolInput<'a> {
        file_path: &'a Path,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: &'a Option<String>,
    }
    #[derive(Serialize)]
    struct Payload<'a> {
        tool_name: &'static str,
        tool_input: ToolInput<'a>,
    }
    if !input_fits(
        &input.file_path,
        &[
            input.content.as_deref().unwrap_or_default(),
            input.old_string.as_deref().unwrap_or_default(),
            input.new_string.as_deref().unwrap_or_default(),
        ],
    ) {
        return Err(unavailable(
            UnavailableReason::InputTooLarge,
            "checker input exceeds 2 MiB",
        ));
    }
    let mut output = BoundedPayload(Vec::new());
    serde_json::to_writer(
        &mut output,
        &Payload {
            tool_name: input.tool_name,
            tool_input: ToolInput {
                file_path: &input.file_path,
                content: &input.content,
                old_string: &input.old_string,
                new_string: &input.new_string,
            },
        },
    )
    .map_err(|error| {
        unavailable(
            if error.is_io() {
                UnavailableReason::InputTooLarge
            } else {
                UnavailableReason::Io
            },
            error,
        )
    })?;
    Ok(output.0)
}

// Only the supervisor owns the child. Dropping the tool future sends cancellation
// instead of dropping a child without an asynchronous wait/reap.
struct CancelOnDrop(Option<oneshot::Sender<()>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

struct SupervisorFinished(watch::Sender<usize>);
impl Drop for SupervisorFinished {
    fn drop(&mut self) {
        self.0.send_modify(|active| *active -= 1);
    }
}

// Same process-group strategy as shell; keeping it local avoids introducing a
// general process framework for this single fixed external command.
struct ProcessGroup(Option<rustix::process::Pid>);
impl ProcessGroup {
    fn kill(&self) {
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn capture_stderr(mut reader: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(MAX_STDERR_BYTES - retained.len());
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < n;
    }
    let mut message = String::from_utf8_lossy(&retained).trim().to_owned();
    if truncated {
        message.push_str("\n[comment-checker stderr truncated after 65536 bytes]");
    }
    Ok(message)
}

async fn supervise(
    mut command: tokio::process::Command,
    payload: Vec<u8>,
    deadline: tokio::time::Instant,
    mut cancel: oneshot::Receiver<()>,
) -> PostEditOutcome {
    // Cancellation can arrive before this detached task first gets scheduled.
    if !matches!(cancel.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
        return unavailable(UnavailableReason::Io, "checker cancelled before launch");
    }
    if tokio::time::Instant::now() >= deadline {
        return unavailable(
            UnavailableReason::Timeout,
            "checker deadline elapsed before launch",
        );
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return unavailable(UnavailableReason::SpawnFailed, error),
    };
    let group = ProcessGroup(
        child
            .id()
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
    );
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let result = {
        let completion = async {
            let (Some(mut stdin), Some(mut stdout), Some(stderr)) = pipes else {
                return Err(io::Error::other("missing checker pipe"));
            };
            let write = async {
                let result = stdin.write_all(&payload).await;
                drop(stdin);
                // Unsupported files may be deliberately ignored by the CLI.
                match result {
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                    result => result,
                }
            };
            let discard = async { tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await };
            let wait = async {
                let status = child.wait().await?;
                // A leader can exit while descendants still hold the pipes open.
                group.kill();
                Ok::<_, io::Error>(status)
            };
            let (_, _, stderr, status) =
                tokio::try_join!(write, discard, capture_stderr(stderr), wait)?;
            Ok((status, stderr))
        };
        tokio::select! {
            _ = &mut cancel => unavailable(UnavailableReason::Io, "checker cancelled"),
            result = tokio::time::timeout_at(deadline, completion) => match result {
                Err(_) => unavailable(UnavailableReason::Timeout, "checker exceeded its configured deadline"),
                Ok(Err(error)) => unavailable(UnavailableReason::Io, error),
                Ok(Ok((status, stderr))) => match status.code() {
                    Some(0) => PostEditOutcome::Pass,
                    Some(2) => PostEditOutcome::Warning { message: stderr },
                    code => unavailable(UnavailableReason::UnexpectedExitCode(code.unwrap_or(-1)),
                        format!("checker exited with {status}: {stderr}")),
                },
            },
        }
    };
    // Includes timeout, cancellation, IO failure, and normal leader exit. Never
    // return until the direct child has been reaped; group cleanup precedes wait.
    group.kill();
    let _ = child.start_kill();
    if let Err(error) = child.wait().await {
        return unavailable(
            UnavailableReason::Io,
            format!("checker cleanup failed: {error}"),
        );
    }
    result
}

#[async_trait]
impl PostEditHook for CommentChecker {
    async fn check(&self, input: &PostEditInput) -> PostEditOutcome {
        let payload = match payload(input) {
            Ok(payload) => payload,
            Err(outcome) => return outcome,
        };
        let Some(program) = self.program.to_str() else {
            return unavailable(UnavailableReason::Io, "checker path is not UTF-8");
        };
        let cwd = input
            .file_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let wrapped = match self.sandbox.wrap(CommandSpec {
            program: program.to_owned(),
            args: vec!["check".into(), "--prompt".into(), self.prompt.clone()],
            cwd: Some(cwd.to_path_buf()),
            extra_env: Vec::new(),
        }) {
            Ok(wrapped) => wrapped,
            Err(error) => return unavailable(UnavailableReason::Io, error),
        };
        let mut command = tokio::process::Command::new(&wrapped.program);
        command.args(&wrapped.args);
        wrapped.apply_environment(command.as_std_mut());
        if let Some(cwd) = &wrapped.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let (cancel, cancelled) = oneshot::channel();
        let mut cancellation = CancelOnDrop(Some(cancel));
        let (send, receive) = oneshot::channel();
        self.supervisors.send_modify(|active| *active += 1);
        let finished = SupervisorFinished(self.supervisors.clone());
        tokio::spawn(async move {
            let _finished = finished;
            let outcome = supervise(command, payload, deadline, cancelled).await;
            let _ = send.send(outcome);
        });
        let result = receive
            .await
            .unwrap_or_else(|_| unavailable(UnavailableReason::Io, "checker supervisor stopped"));
        cancellation.0.take();
        result
    }

    async fn drain(&self) {
        let mut supervisors = self.supervisors.subscribe();
        loop {
            if *supervisors.borrow_and_update() == 0 {
                return;
            }
            if supervisors.changed().await.is_err() {
                return;
            }
        }
    }

    fn take_unavailable_notification(&self) -> bool {
        !self.unavailable_notified.swap(true, Ordering::Relaxed)
    }
}

pub(crate) fn apply_unavailable(
    hook: &dyn PostEditHook,
    reason: UnavailableReason,
    detail: impl ToString,
    result: &mut ToolResult,
) {
    if !hook.take_unavailable_notification() {
        return;
    }
    let detail = detail.to_string();
    tracing::warn!(?reason, %detail, "comment-checker unavailable");
    result.detail.get_or_insert_with(|| serde_json::json!({}))["comment_checker"] =
        serde_json::json!({"outcome": "unavailable", "reason": reason, "detail": detail});
}

fn warning_preview(message: &str) -> &str {
    let mut end = message.len().min(WARNING_PREVIEW_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    if let Some((index, _)) = message[..end]
        .match_indices('\n')
        .nth(WARNING_PREVIEW_LINES - 1)
    {
        end = index;
    }
    &message[..end]
}

fn quoted_warning(message: &str, truncated: bool) -> String {
    let mut warning = String::from(
        "\n[comment-checker warning]\nExternal comment-checker diagnostic (untrusted; quoted):\n",
    );
    for line in message.lines() {
        warning.push_str("> ");
        warning.push_str(line);
        warning.push('\n');
    }
    if truncated {
        warning.push_str("> [diagnostic preview truncated; see output artifact]\n");
    }
    warning.push_str("[end comment-checker warning]");
    warning
}

pub(crate) async fn apply_post_edit(
    hook: &dyn PostEditHook,
    input: &PostEditInput,
    result: &mut ToolResult,
) {
    match hook.check(input).await {
        PostEditOutcome::Pass => {}
        PostEditOutcome::Warning { message } => {
            // Bound before quoting, not after: a generic tail preview must never
            // expose a diagnostic line with its quote prefix or header cut off.
            let message = crate::sanitize::escape_control_markers(&message);
            let safe = secret_guard::SecretRedactor::from_env().redact(&message);
            let preview = warning_preview(&safe.text);
            let truncated = preview.len() < safe.text.len();
            let warning = quoted_warning(preview, truncated);
            let mut detail = serde_json::json!({"outcome": "warning", "message": preview});
            if truncated {
                let full = quoted_warning(&safe.text, false);
                let artifact = crate::output::artifact_result(
                    &full,
                    &warning,
                    full.len() as u64,
                    true,
                    safe.count,
                );
                result.content.push_str(&artifact.content);
                detail["output_artifact"] = artifact.detail.unwrap()["output_artifact"].clone();
            } else {
                result.content.push_str(&warning);
            }
            result.detail.get_or_insert_with(|| serde_json::json!({}))["comment_checker"] = detail;
        }
        PostEditOutcome::Unavailable { reason, detail } => {
            apply_unavailable(hook, reason, detail, result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn path_search_finds_first_executable_and_skips_invalid_entries() {
        let dir = tempfile::tempdir().unwrap();
        let roots: Vec<_> = ["absent", "non-executable", "directory", "first", "second"]
            .map(|part| dir.path().join(part))
            .into();
        for root in &roots {
            std::fs::create_dir(root).unwrap();
        }
        std::fs::write(roots[1].join("checker"), "not executable").unwrap();
        std::fs::create_dir(roots[2].join("checker")).unwrap();
        executable(&roots[3].join("checker"));
        executable(&roots[4].join("checker"));
        let path = std::env::join_paths(&roots).unwrap();
        assert_eq!(
            resolve_in_path("checker", Some(&path), &[]),
            Some(roots[3].join("checker").canonicalize().unwrap())
        );
        assert_eq!(resolve_in_path("missing", Some(&path), &[]), None);
        assert_eq!(resolve_in_path("checker", None, &[]), None);
    }

    #[test]
    fn relative_and_empty_path_entries_and_explicit_relative_paths_are_rejected() {
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&cwd).unwrap();
        executable(&dir.path().join("checker"));
        let relative = dir.path().strip_prefix(&cwd).unwrap();
        let path = std::env::join_paths([relative]).unwrap();
        assert_eq!(resolve_in_path("checker", Some(&path), &[]), None);
        let local = tempfile::NamedTempFile::new_in(&cwd).unwrap();
        executable(local.path());
        let name = local.path().file_name().unwrap().to_str().unwrap();
        assert_eq!(
            resolve_in_path(name, Some(std::ffi::OsStr::new("")), &[]),
            None
        );
        assert_eq!(executable_path(&relative.join("checker"), &[]), None);
    }

    #[test]
    fn path_search_supports_spaces_symlinks_and_non_utf8_path_entries() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let unusual = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"space \xff".to_vec()));
        std::fs::create_dir(&unusual).unwrap();
        executable(&dir.path().join("real checker"));
        std::os::unix::fs::symlink(dir.path().join("real checker"), unusual.join("checker"))
            .unwrap();
        let path = std::env::join_paths([unusual]).unwrap();
        assert_eq!(
            resolve_in_path("checker", Some(&path), &[]),
            Some(dir.path().join("real checker").canonicalize().unwrap())
        );
    }

    #[test]
    fn project_candidates_and_symlinks_in_either_direction_are_forbidden() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let worktree = dir.path().join("worktree");
        let installed = dir.path().join("installed");
        for root in [&project, &worktree, &installed] {
            std::fs::create_dir(root).unwrap();
            executable(&root.join("checker"));
        }
        let roots = verified_roots(&[project.clone(), worktree.clone()]).unwrap();
        std::os::unix::fs::symlink(project.join("checker"), installed.join("project-link"))
            .unwrap();
        std::os::unix::fs::symlink(installed.join("checker"), worktree.join("external-link"))
            .unwrap();
        for path in [
            project.join("checker"),
            worktree.join("checker"),
            installed.join("project-link"),
            worktree.join("external-link"),
        ] {
            assert_eq!(executable_path(&path, &roots), None, "{}", path.display());
        }
        let path = std::env::join_paths([&project, &worktree, &installed]).unwrap();
        assert_eq!(
            resolve_in_path("checker", Some(&path), &roots),
            Some(installed.join("checker"))
        );
        assert!(verified_roots(&[dir.path().join("missing")]).is_none());
        assert!(verified_roots(&[PathBuf::from("relative")]).is_none());
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&project, &alias).unwrap();
        let project_outside_link = project.join("external-link");
        std::os::unix::fs::symlink(installed.join("checker"), &project_outside_link).unwrap();
        assert_eq!(executable_path(&alias.join("external-link"), &roots), None);
        let aliased_roots = verified_roots(&[alias]).unwrap();
        assert_eq!(
            executable_path(&project.join("checker"), &aliased_roots),
            None
        );
    }
}
