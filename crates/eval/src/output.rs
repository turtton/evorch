//! The files an evaluation leaves behind (ADR 0036 §6): one `manifest.json`
//! for the whole evaluation and, per replay, `runs/<label>-<n>/` holding its
//! `evorch.db`, `diff.patch`, `conversation.json` and `outcome.json`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use config::{EvalBudgetConfig, RoleBindingConfig};
use serde::{Deserialize, Serialize};

use crate::changes::Changes;
use crate::conversation::Entry;
use crate::driver::{DeniedRequest, Status};
use crate::metrics::ModelUse;
use crate::pins::Pins;
use crate::roles::SwappedRole;
use crate::source::{OriginalThread, UserTurn};
use crate::verify::Verification;
use crate::workspace::RefChange;
use storage::eval_sample::EvalSample;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const DATABASE_FILE: &str = "evorch.db";
pub const DIFF_FILE: &str = "diff.patch";
pub const CONVERSATION_FILE: &str = "conversation.json";
pub const OUTCOME_FILE: &str = "outcome.json";
/// The replay's initial context as its own runtime composed it.
pub const BASE_CONTEXT_FILE: &str = "base-context.json";
pub const REPORT_JSON_FILE: &str = "report.json";
pub const REPORT_MARKDOWN_FILE: &str = "report.md";
pub const JUDGMENTS_DIR: &str = "judgments";

/// `<user config dir>/eval/<UTC YYYYmmdd-HHMMSS>-<thread id>`.
pub fn default_dir(user_config_dir: &Path, thread_id: &str, now: SystemTime) -> PathBuf {
    let stamp = DateTime::<Utc>::from(now).format("%Y%m%d-%H%M%S");
    let thread: String = thread_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    user_config_dir
        .join("eval")
        .join(format!("{stamp}-{thread}"))
}

/// Create `dir`, refusing one that already holds anything so earlier results
/// are never mixed with or overwritten by a new evaluation.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("output directory {} is not empty", dir.display()),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(dir),
        Err(error) => Err(error),
    }
}

pub fn timestamp(at: SystemTime) -> String {
    DateTime::<Utc>::from(at).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartSummary {
    pub project_root: PathBuf,
    pub repository: PathBuf,
    pub repo_prefix: String,
    pub head_commit: String,
    pub head_ref: Option<String>,
    pub diff_bytes: usize,
    pub untracked_entries: usize,
    pub captured_at_ns: i64,
}

impl StartSummary {
    pub fn of(thread: &OriginalThread) -> Self {
        Self {
            project_root: thread.project_root.clone(),
            repository: thread.source_toplevel.clone(),
            repo_prefix: thread.snapshot.repo_prefix.clone(),
            head_commit: thread.snapshot.head_commit.clone(),
            head_ref: thread.snapshot.head_ref.clone(),
            diff_bytes: thread.snapshot.diff.len(),
            untracked_entries: thread.snapshot.untracked.len(),
            captured_at_ns: thread.captured_at_ns,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindingSummary {
    /// Where the binding came from: its `eval` key, or the role binding used
    /// because that key is unset.
    pub source: String,
    pub binding: RoleBindingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEntry {
    pub label: String,
    pub index: u32,
    pub dir: String,
    /// The random run identifier placed in the system prompt, so cached
    /// answers from another replay cannot be reused.
    pub nonce: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: Option<Status>,
    /// What the replay actually used, read from its database when it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<Observed>,
}

/// The models and frozen tool descriptions a replay's runs really used.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observed {
    pub models: Vec<ModelUse>,
    /// Only runs that froze a tool description snapshot are listed.
    pub tool_descriptions: Vec<AppliedDescriptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedDescriptions {
    pub run_id: String,
    pub role: String,
    pub profile: Option<String>,
    pub route_model_id: Option<String>,
    /// The snapshot's own hash, as the runtime froze it.
    pub hash: String,
    /// The tools the snapshot overrides; empty when it is the empty snapshot.
    pub tools: Vec<String>,
    /// The tools this run's runtime offered the model, from the run's own
    /// persisted record; `None` when the record does not say, in which case
    /// nothing can be said to have been offered.
    #[serde(default)]
    pub offered: Option<Vec<String>>,
}

/// The size of what a replay's root run starts with before any task text: its
/// system prompt and its tools, as the replay's own runtime composed them with
/// the configuration of that side. The tokens are the runtime's estimate
/// (bytes / 4), not a provider count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseContextSummary {
    pub role: String,
    pub category: Option<String>,
    /// `profile/model` the run starts on.
    pub selected_model: String,
    pub system_bytes: u64,
    pub system_tokens: u64,
    pub tools_bytes: u64,
    pub tools_tokens: u64,
    pub tool_count: usize,
    /// Each tool's own estimated tokens, so a changed description shows where.
    pub tools: Vec<ToolSize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSize {
    pub name: String,
    pub description_bytes: u64,
    pub tokens: u64,
}

impl BaseContextSummary {
    pub fn total_tokens(&self) -> u64 {
        self.system_tokens.saturating_add(self.tools_tokens)
    }

    pub fn of(report: &runtime::base_context::BaseContextReport) -> Self {
        let size = |text: &str| u64::try_from(text.len()).unwrap_or(u64::MAX);
        let tools: Vec<ToolSize> = report
            .tools
            .iter()
            .map(|tool| ToolSize {
                name: tool.name.clone(),
                description_bytes: size(&tool.description),
                tokens: tool.estimated_tokens,
            })
            .collect();
        Self {
            role: report.role.name().to_owned(),
            category: report.category.clone(),
            selected_model: report.selected_model.clone(),
            system_bytes: report
                .sections
                .iter()
                .map(|section| size(&section.text))
                .sum(),
            system_tokens: report.section_tokens(),
            tools_bytes: report
                .tools
                .iter()
                .map(|tool| size(&tool.description) + size(&tool.input_schema.to_string()))
                .sum(),
            tools_tokens: report.tool_tokens(),
            tool_count: tools.len(),
            tools,
        }
    }
}

/// The sample an evaluation was started for, as it was when the evaluation
/// started: later edits of the sample do not change what this evaluation is
/// graded against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinnedSample {
    /// [`EvalSample::content_hash`] of `sample`.
    pub hash: String,
    pub sample: EvalSample,
}

impl PinnedSample {
    pub fn of(sample: EvalSample) -> Self {
        Self {
            hash: sample.content_hash(),
            sample,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub thread_id: String,
    pub source_db: PathBuf,
    pub start: StartSummary,
    /// What the original user said, as the simulated user and the judge see it.
    #[serde(default)]
    pub original_turns: Vec<UserTurn>,
    /// The problem the evaluation is about, judged as its own aspect.
    #[serde(default)]
    pub problem: Option<String>,
    /// The thread's evaluation sample as it was at the start, with its checks;
    /// `None` when the thread had none (no machine judgment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<PinnedSample>,
    /// The verification command run in each replay's final workspace.
    #[serde(default)]
    pub verify: Option<String>,
    /// The candidate overlay as given, or `None` when only the baseline runs.
    pub candidate_overlay: Option<String>,
    /// The overlay both configurations run under, as given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_overlay: Option<String>,
    /// Roles the overlay changes, with the models each side resolves them to.
    /// Role binding differences only: see `changes` for the rest.
    #[serde(default)]
    pub swapped_roles: Vec<SwappedRole>,
    /// Everything the candidate changes: bindings, tool descriptions and the
    /// roles they can reach.
    #[serde(default)]
    pub changes: Changes,
    /// The hashes, build and model identity the evaluation was started on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pins: Option<Pins>,
    pub configurations: Vec<String>,
    pub repeat: u32,
    pub simulator: BindingSummary,
    pub budget: EvalBudgetConfig,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub runs: Vec<RunEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    pub reason: String,
    pub user_turns: u32,
    pub tokens: u64,
    pub tool_calls: u32,
    pub elapsed_ms: u64,
    pub denied_requests: Vec<DeniedRequest>,
    pub pushed_refs: Vec<RefChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
}

/// Write `value` as pretty JSON through a temporary file, so a reader never
/// sees a partly written file.
pub fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let mut text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    text.push(b'\n');
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, path)
}

/// A directory held open by descriptor. Everything done through it is relative
/// to the descriptor and follows no symbolic link, so a link planted in the
/// directory, or the directory swapped for a link after it was chosen, cannot
/// send a read or a write elsewhere.
#[derive(Debug)]
pub struct PinnedDir {
    fd: rustix::fd::OwnedFd,
    path: PathBuf,
}

/// A name that is one ordinary path component.
fn is_plain(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\0'])
}

fn plain(name: &str) -> io::Result<()> {
    if is_plain(name) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a plain file name"),
        ))
    }
}

impl PinnedDir {
    /// Open `path` itself, which is trusted and may be a link: the evaluation
    /// root.
    ///
    /// # Errors
    /// The directory cannot be opened.
    pub fn open_root(path: &Path) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    /// The directory `path` names, refusing one whose last component is a link.
    ///
    /// # Errors
    /// The parent cannot be opened, or the directory is missing or a link.
    pub fn open_path(path: &Path) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "no directory name");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(invalid)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        Self::open_root(parent.unwrap_or_else(|| Path::new(".")))?.open_dir(name)
    }

    /// The directory `name` inside this one, never through a link.
    ///
    /// # Errors
    /// `name` is not a plain name, or it is missing, not a directory or a link.
    pub fn open_dir(&self, name: &str) -> io::Result<Self> {
        use rustix::fs::{Mode, OFlags};
        plain(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            fd,
            path: self.path.join(name),
        })
    }

    /// The directory `relative` (components separated by `/`) below this one,
    /// opened one component at a time; a link or a `..` anywhere in it is refused.
    ///
    /// # Errors
    /// As [`Self::open_dir`] for any component.
    pub fn open_below(&self, relative: &str) -> io::Result<Self> {
        let mut parts = relative.split('/');
        let mut dir = self.open_dir(parts.next().unwrap_or_default())?;
        for part in parts {
            dir = dir.open_dir(part)?;
        }
        Ok(dir)
    }

    /// The directory `name` inside this one, made when it is missing.
    ///
    /// # Errors
    /// As [`Self::open_dir`], or it cannot be made.
    pub fn ensure_dir(&self, name: &str) -> io::Result<Self> {
        use rustix::fs::Mode;
        plain(name)?;
        match rustix::fs::mkdirat(&self.fd, name, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(errno) => return Err(errno.into()),
        }
        self.open_dir(name)
    }

    /// Where the directory is, for messages and for a reader that takes a path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The bytes of the regular file `name`, never through a link and never
    /// blocking on a FIFO or a device.
    ///
    /// # Errors
    /// `NotFound` when there is no such file; otherwise it is a link, not a
    /// regular file or unreadable.
    pub fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        use rustix::fs::{Mode, OFlags};
        use std::io::Read as _;
        plain(name)?;
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut file = std::fs::File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", self.join(name).display()),
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Whether `name` is a regular file here, found without following a link.
    ///
    /// # Errors
    /// The directory cannot be inspected.
    pub fn is_regular_file(&self, name: &str) -> io::Result<bool> {
        plain(name)?;
        match rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(rustix::fs::FileType::from_raw_mode(stat.st_mode)
                == rustix::fs::FileType::RegularFile),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(errno) => Err(errno.into()),
        }
    }

    /// The regular file `name` opened for reading without following a link,
    /// or `None` when it is missing, a link or not a regular file. Reading
    /// the returned file reads what was checked, even if the name is replaced
    /// afterwards.
    ///
    /// # Errors
    /// The file cannot be opened for another reason.
    pub fn open_regular(&self, name: &str) -> io::Result<Option<std::fs::File>> {
        use rustix::fs::{Mode, OFlags};
        plain(name)?;
        let opened = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        );
        let file = match opened {
            Ok(fd) => std::fs::File::from(fd),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::LOOP) => return Ok(None),
            Err(errno) => return Err(errno.into()),
        };
        Ok(file.metadata()?.is_file().then_some(file))
    }

    /// Put `bytes` in `name` through a new temporary file renamed over it. A
    /// link that is already called `name` is replaced, not followed.
    ///
    /// # Errors
    /// The file cannot be written.
    pub fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        use std::io::Write as _;
        plain(name)?;
        let partial = format!(".{name}.partial");
        match rustix::fs::unlinkat(&self.fd, &partial, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(errno) => return Err(errno.into()),
        }
        let fd = rustix::fs::openat(
            &self.fd,
            &partial,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        )?;
        let mut file = std::fs::File::from(fd);
        let written = file.write_all(bytes).and_then(|()| file.flush());
        drop(file);
        let renamed = written.and_then(|()| {
            rustix::fs::renameat(&self.fd, &partial, &self.fd, name).map_err(Into::into)
        });
        if renamed.is_err() {
            let _ = rustix::fs::unlinkat(&self.fd, &partial, AtFlags::empty());
        }
        renamed
    }

    /// [`Self::write`] that never replaces a file: the bytes reach `name` only
    /// when nothing is there, atomically, so a reader never sees a part of
    /// them.
    ///
    /// # Errors
    /// `AlreadyExists` when `name` is taken (nothing is changed then); else as
    /// [`Self::write`].
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        use rustix::fs::{AtFlags, Mode, OFlags};
        use std::io::Write as _;
        plain(name)?;
        let partial = format!(".{name}.partial");
        match rustix::fs::unlinkat(&self.fd, &partial, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(errno) => return Err(errno.into()),
        }
        let fd = rustix::fs::openat(
            &self.fd,
            &partial,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH,
        )?;
        let mut file = std::fs::File::from(fd);
        let written = file.write_all(bytes).and_then(|()| file.flush());
        drop(file);
        let linked = written.and_then(|()| {
            rustix::fs::linkat(&self.fd, &partial, &self.fd, name, AtFlags::empty())
                .map_err(Into::into)
        });
        let _ = rustix::fs::unlinkat(&self.fd, &partial, AtFlags::empty());
        linked
    }

    /// [`Self::write_new`] of `value` as pretty JSON.
    ///
    /// # Errors
    /// As [`Self::write_new`].
    pub fn write_json_new(&self, name: &str, value: &impl Serialize) -> io::Result<()> {
        let mut text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        text.push(b'\n');
        self.write_new(name, &text)
    }

    /// [`Self::write`] of `value` as pretty JSON.
    ///
    /// # Errors
    /// As [`Self::write`].
    pub fn write_json(&self, name: &str, value: &impl Serialize) -> io::Result<()> {
        let mut text = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
        text.push(b'\n');
        self.write(name, &text)
    }
}

pub fn write_run(
    dir: &Path,
    outcome: &Outcome,
    conversation: &[Entry],
    diff: Option<&[u8]>,
) -> io::Result<()> {
    if let Some(diff) = diff {
        std::fs::write(dir.join(DIFF_FILE), diff)?;
    }
    write_json(&dir.join(CONVERSATION_FILE), &conversation)?;
    write_json(&dir.join(OUTCOME_FILE), outcome)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn default_dir_is_stamped_in_utc_and_keeps_thread_ids_path_safe() {
        // Given: a fixed instant and a thread id with path separators.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_454_400);

        // When: the default output directory is derived.
        let dir = default_dir(Path::new("/config"), "a/b c", at);

        // Then: it sits under eval/ with a UTC stamp and a sanitized id.
        assert_eq!(dir, Path::new("/config/eval/20261020-000000-a_b_c"));
    }

    #[test]
    fn only_missing_or_empty_output_directories_are_used() {
        // Given: a missing, an empty and a non-empty directory.
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("new/eval");
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let used = root.path().join("used");
        std::fs::create_dir(&used).unwrap();
        std::fs::write(used.join(MANIFEST_FILE), "{}").unwrap();

        // When / Then: the first two are accepted and the last is refused.
        create_dir(&missing).unwrap();
        assert!(missing.is_dir());
        create_dir(&empty).unwrap();
        assert_eq!(
            create_dir(&used).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    /// A link among the files of an evaluation directory is replaced, never
    /// followed, and a link standing in for a directory or a file to read is
    /// refused; this is what keeps a report inside the evaluation root.
    #[test]
    fn links_inside_a_pinned_directory_are_never_followed() {
        use std::os::unix::fs::symlink;

        // Given: report files and the judgments directory are links to places outside.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("eval");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, "untouched").unwrap();
        symlink(&victim, dir.join(REPORT_MARKDOWN_FILE)).unwrap();
        symlink(&victim, dir.join(REPORT_JSON_FILE)).unwrap();
        symlink(&outside, dir.join(JUDGMENTS_DIR)).unwrap();
        let pinned = PinnedDir::open_path(&dir).unwrap();

        // When: the report files are written and the links are used as a directory and a file.
        pinned
            .write_json(REPORT_JSON_FILE, &serde_json::json!({}))
            .unwrap();
        pinned.write(REPORT_MARKDOWN_FILE, b"# report").unwrap();

        // Then: the outside file is untouched, the files are regular, and reading refuses links.
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        for name in [REPORT_MARKDOWN_FILE, REPORT_JSON_FILE] {
            assert!(std::fs::symlink_metadata(dir.join(name)).unwrap().is_file());
        }
        assert!(pinned.open_dir(JUDGMENTS_DIR).is_err());
        assert!(pinned.ensure_dir(JUDGMENTS_DIR).is_err());
        symlink(&victim, dir.join("linked.json")).unwrap();
        assert!(pinned.read("linked.json").is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    /// A directory swapped for a link after it was pinned is not followed: what
    /// is written goes where the descriptor points.
    #[test]
    fn a_directory_swapped_for_a_link_after_it_was_pinned_is_not_followed() {
        use std::os::unix::fs::symlink;

        // Given: the directory is pinned, and then replaced by a link to another place.
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("eval");
        let moved = root.path().join("moved");
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let pinned = PinnedDir::open_path(&dir).unwrap();
        std::fs::rename(&dir, &moved).unwrap();
        symlink(&elsewhere, &dir).unwrap();

        // When: files are written and a subdirectory is made through the pinned directory.
        pinned.write(REPORT_MARKDOWN_FILE, b"# report").unwrap();
        pinned.ensure_dir(JUDGMENTS_DIR).unwrap();

        // Then: they are in the directory that was pinned, and nowhere else.
        assert!(moved.join(REPORT_MARKDOWN_FILE).is_file());
        assert!(moved.join(JUDGMENTS_DIR).is_dir());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    /// A directory that is itself a link, or a name that climbs out, is refused.
    #[test]
    fn a_directory_that_is_a_link_or_climbs_out_cannot_be_opened() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, root.path().join("link")).unwrap();
        let pinned = PinnedDir::open_root(root.path()).unwrap();
        assert!(pinned.open_dir("real").is_ok());
        assert!(pinned.open_dir("link").is_err());
        assert!(PinnedDir::open_path(&root.path().join("link")).is_err());
        assert!(pinned.open_dir("../real").is_err());
        assert!(pinned.open_below("real/../real").is_err());
    }
}

#[cfg(test)]
mod write_new_tests {
    use super::*;

    #[test]
    fn write_new_creates_a_file_and_refuses_to_replace_one() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = PinnedDir::open_path(dir.path()).unwrap();

        pinned.write_new("result.json", b"first").unwrap();
        let again = pinned.write_new("result.json", b"second").unwrap_err();

        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(dir.path().join("result.json")).unwrap(),
            b"first"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, ["result.json"]);
    }
}
