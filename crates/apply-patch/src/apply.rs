//! Read-only planning followed by per-file atomic commits.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::file_update::derive_new_contents_from_chunks;
use crate::{ApplyPatchError, ErrorClass, Hunk, IoError, parse_patch, targets};

/// The final, resolved path and its last patch spelling used to report a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedChange {
    pub path: PathBuf,
    pub raw: PathBuf,
    pub change: ChangeKind,
}

/// A before/after delta, with moves stored at their final destination path.
///
/// `old` and `overwritten` refer to the initial disk snapshot, not an
/// intermediate virtual state. A completed move also represents removal of
/// `from`; an incomplete move is instead reported as an add/update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added {
        content: String,
        overwritten: Option<String>,
    },
    Updated {
        old: String,
        new: String,
    },
    Moved {
        from: PathBuf,
        old: String,
        new: String,
        overwritten: Option<String>,
    },
    Deleted {
        old: String,
    },
}

impl AppliedChange {
    fn letter(&self) -> char {
        match self.change {
            ChangeKind::Added { .. } => 'A',
            ChangeKind::Updated { .. } | ChangeKind::Moved { .. } => 'M',
            ChangeKind::Deleted { .. } => 'D',
        }
    }
}

/// Results describe final disk mutations, not intermediate hunk operations.
///
/// `changes` aggregates repeated operations to one before/after change per
/// final path; a completed move also accounts for removal of its source.
/// In contrast, `summary` lists every hunk (including no-ops), in upstream's
/// added/modified/deleted order, using the paths as written in the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub changes: Vec<AppliedChange>,
    pub summary: String,
}

/// An I/O error and only the mutations actually persisted before it occurred.
///
/// `committed` uses the same final-path aggregation as [`Applied::changes`],
/// unlike the per-hunk success summary in [`Applied::summary`].
/// No rollback is attempted. A move whose destination was written but whose
/// source was not removed is reported as an add/update, not a completed move.
/// Parent directories created during commit can remain after a failure; they
/// are not textual file changes and are not included in `committed`.
#[derive(Debug)]
pub struct CommitFailure {
    pub error: ApplyPatchError,
    pub committed: Vec<AppliedChange>,
}

impl fmt::Display for CommitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;
        if !self.committed.is_empty() {
            writeln!(f, "\nApplied before the failure:")?;
            for change in &self.committed {
                writeln!(f, "{} {}", change.letter(), change.raw.display())?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for CommitFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl CommitFailure {
    pub fn class(&self) -> ErrorClass {
        self.error.class()
    }
}

/// The convenience API retains whether failure occurred before or during commit.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Plan(#[from] ApplyPatchError),
    #[error(transparent)]
    Commit(#[from] CommitFailure),
}

impl ApplyError {
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Plan(error) => error.class(),
            Self::Commit(failure) => failure.class(),
        }
    }
}

/// Plan without writing, then commit. See [`Plan::commit`] for its guarantees.
pub fn apply_patch(patch: &str, root: Option<&Path>) -> Result<Applied, ApplyError> {
    Ok(plan(patch, root)?.commit()?)
}

#[derive(Debug, Clone)]
enum VfsState {
    Absent,
    Text {
        content: String,
        mode: Option<Permissions>,
        origin: PathBuf,
    },
}

impl VfsState {
    fn content(&self) -> Option<&str> {
        match self {
            Self::Absent => None,
            Self::Text { content, .. } => Some(content),
        }
    }

    fn mode(&self) -> Option<Permissions> {
        match self {
            Self::Absent => None,
            Self::Text { mode, .. } => mode.clone(),
        }
    }

    fn same_file(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Absent, Self::Absent) => true,
            (
                Self::Text {
                    content: a,
                    mode: am,
                    ..
                },
                Self::Text {
                    content: b,
                    mode: bm,
                    ..
                },
            ) => a == b && am == bm,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct VfsEntry {
    initial: VfsState,
    current: VfsState,
    raw: PathBuf,
    added: bool,
    remove_context: &'static str,
    initial_link: bool,
}

/// A validated, read-only snapshot of the initial and intended final states.
///
/// Planning never creates directories, temporary files, or changes permissions.
/// The caller must serialize competing edits between planning and commit: this
/// type does not lock files or detect external modifications after planning.
#[derive(Debug)]
pub struct Plan {
    files: BTreeMap<PathBuf, VfsEntry>,
    // Final symlinks are directory entries, not duplicate content snapshots.
    // Removing one drops this edge without removing its referent's VFS state.
    links: BTreeMap<PathBuf, PathBuf>,
    summary: String,
}

/// Parse and validate every path, then apply hunks in order on a virtual FS.
/// Relative paths need `root`; path resolution is shared with [`targets`].
/// Parse, path, matching, and file-read failures leave the filesystem untouched.
pub fn plan(patch: &str, root: Option<&Path>) -> Result<Plan, ApplyPatchError> {
    let hunks = parse_patch(patch)?.hunks;
    let resolved = targets(patch, root)?;
    if hunks.is_empty() {
        return Err(ApplyPatchError::NoFilesModified);
    }
    let mut plan = Plan {
        files: BTreeMap::new(),
        links: BTreeMap::new(),
        summary: String::new(),
    };
    let mut paths = resolved.into_iter();
    let mut reports = [Vec::new(), Vec::new(), Vec::new()];
    for hunk in hunks {
        // targets() returns one source for each hunk, plus one move destination.
        let target = paths.next().expect("each hunk has a target");
        let path = target.resolved;
        match hunk {
            Hunk::AddFile { contents, .. } => {
                let path = plan.resolve(&path, &target.raw, "Failed to write file", true)?;
                let entry = plan.load(&path, &target.raw, "Failed to write file")?;
                let mode = entry.current.mode().or_else(|| entry.initial.mode());
                entry.current = VfsState::Text {
                    content: contents,
                    mode,
                    origin: path.clone(),
                };
                entry.added = true;
                reports[0].push(target.raw);
            }
            Hunk::DeleteFile { .. } => {
                let entry_path =
                    plan.resolve(&path, &target.raw, "Failed to delete file", false)?;
                let content_path =
                    plan.resolve(&path, &target.raw, "Failed to delete file", true)?;
                let content = plan.load(&content_path, &target.raw, "Failed to delete file")?;
                require_text(&content.current, &path, "Failed to delete file")?;
                let entry = plan.load(&entry_path, &target.raw, "Failed to delete file")?;
                entry.current = VfsState::Absent;
                entry.remove_context = "Failed to delete file";
                plan.links.remove(&entry_path);
                reports[2].push(target.raw);
            }
            Hunk::UpdateFile {
                move_path, chunks, ..
            } => {
                let entry_path =
                    plan.resolve(&path, &target.raw, "Failed to read file to update", false)?;
                let content_path =
                    plan.resolve(&path, &target.raw, "Failed to read file to update", true)?;
                let entry =
                    plan.load(&content_path, &target.raw, "Failed to read file to update")?;
                let old = require_text(&entry.current, &path, "Failed to read file to update")?;
                let new = derive_new_contents_from_chunks(old, &path.to_string_lossy(), &chunks)?;
                let VfsState::Text { mode, origin, .. } = &entry.current else {
                    unreachable!()
                };
                let mut updated = VfsState::Text {
                    content: new,
                    mode: mode.clone(),
                    origin: origin.clone(),
                };
                if move_path.is_some() {
                    let dest = paths.next().expect("moves have a destination");
                    let mut destination_uses_source = false;
                    let dest_path = plan.resolve_observing(
                        &dest.resolved,
                        &dest.raw,
                        "Failed to write file",
                        true,
                        |entry| destination_uses_source |= entry == entry_path,
                    )?;
                    // Unlinking an entry used to resolve the destination would
                    // discard the write or leave the destination dangling.
                    if !destination_uses_source {
                        if entry_path != content_path
                            && let VfsState::Text { origin, .. } = &mut updated
                        {
                            *origin = entry_path.clone();
                        }
                        let destination =
                            plan.load(&dest_path, &dest.raw, "Failed to write file")?;
                        destination.current = updated;
                        destination.added = false;
                        let source =
                            plan.load(&entry_path, &target.raw, "Failed to remove original")?;
                        source.current = VfsState::Absent;
                        source.remove_context = "Failed to remove original";
                        plan.links.remove(&entry_path);
                    } else {
                        let entry = plan.load(&content_path, &dest.raw, "Failed to write file")?;
                        entry.current = updated;
                    }
                    reports[1].push(dest.raw);
                } else {
                    entry.current = updated;
                    reports[1].push(target.raw);
                }
            }
        }
    }
    plan.summary
        .push_str("Success. Updated the following files:\n");
    for (letter, paths) in ['A', 'M', 'D'].into_iter().zip(reports) {
        for path in paths {
            plan.summary
                .push_str(&format!("{letter} {}\n", path.display()));
        }
    }
    Ok(plan)
}

impl Plan {
    /// Content operations follow final links; unlink operations stop at the
    /// canonical parent plus filename. Both use the same virtual link topology.
    fn resolve(
        &mut self,
        path: &Path,
        raw: &Path,
        context: &str,
        follow: bool,
    ) -> Result<PathBuf, ApplyPatchError> {
        self.resolve_observing(path, raw, context, follow, |_| {})
    }

    fn resolve_observing(
        &mut self,
        path: &Path,
        raw: &Path,
        context: &str,
        follow: bool,
        mut visit: impl FnMut(&Path),
    ) -> Result<PathBuf, ApplyPatchError> {
        let mut current = path.to_path_buf();
        for _ in 0..=40 {
            let entry =
                entry_destination(&current).map_err(|error| io_error(context, path, error))?;
            visit(&entry);
            if !self.files.contains_key(&entry)
                && fs::symlink_metadata(&entry).is_ok_and(|metadata| metadata.is_symlink())
            {
                self.load(&entry, raw, context)?;
                let target =
                    fs::read_link(&entry).map_err(|error| io_error(context, path, error))?;
                self.links.insert(
                    entry.clone(),
                    entry.parent().unwrap_or(Path::new(".")).join(target),
                );
            }
            if follow && let Some(target) = self.links.get(&entry) {
                current = target.clone();
            } else {
                return Ok(entry);
            }
        }
        Err(io_error(
            context,
            path,
            io::Error::other("too many levels of symbolic links"),
        ))
    }

    fn load(
        &mut self,
        path: &Path,
        raw: &Path,
        context: &str,
    ) -> Result<&mut VfsEntry, ApplyPatchError> {
        if !self.files.contains_key(path) {
            let initial = read_state(path).map_err(|error| io_error(context, path, error))?;
            self.files.insert(
                path.to_path_buf(),
                VfsEntry {
                    current: initial.clone(),
                    initial,
                    raw: raw.to_path_buf(),
                    added: false,
                    remove_context: "Failed to delete file",
                    initial_link: fs::symlink_metadata(path)
                        .is_ok_and(|metadata| metadata.is_symlink()),
                },
            );
        }
        let entry = self.files.get_mut(path).expect("entry was initialized");
        entry.raw = raw.to_path_buf();
        Ok(entry)
    }

    /// Write final differences, in resolved-path order, before removing files.
    ///
    /// Each write uses a same-directory temporary file and atomic `persist`.
    /// Existing permissions are preserved; moves inherit source permissions.
    /// Temporary files are removed on every failure path. This is not a
    /// multi-file transaction: I/O failures return the precise committed prefix.
    pub fn commit(self) -> Result<Applied, CommitFailure> {
        let mut committed = Vec::new();
        for (path, entry) in &self.files {
            if self.links.contains_key(path)
                || (entry.initial.same_file(&entry.current) && !entry.initial_link)
            {
                continue;
            }
            let VfsState::Text { content, mode, .. } = &entry.current else {
                continue;
            };
            if let Err(error) = write_atomically(path, content, mode.clone()) {
                return Err(CommitFailure { error, committed });
            }
            committed.push(Self::write_change(path, entry));
        }
        for (path, entry) in &self.files {
            if self.links.contains_key(path)
                || (entry.initial.content().is_none() && !entry.initial_link)
                || !matches!(entry.current, VfsState::Absent)
            {
                continue;
            }
            if let Err(error) = fs::remove_file(path) {
                return Err(CommitFailure {
                    error: io_error(entry.remove_context, path, error),
                    committed,
                });
            }
            // Only collapse to Moved after the source has actually been removed.
            let moved_to = self.files.iter().find(|(dest, candidate)| {
                *dest != path
                    && matches!(&candidate.current, VfsState::Text { origin, .. } if origin == path)
            });
            if let Some((dest, candidate)) = moved_to
                && let Some(change) = committed.iter_mut().find(|change| &change.path == dest)
            {
                change.change = ChangeKind::Moved {
                    from: path.clone(),
                    old: entry.initial.content().unwrap_or_default().to_string(),
                    new: candidate.current.content().unwrap().to_string(),
                    overwritten: candidate.initial.content().map(str::to_string),
                };
                continue;
            }
            committed.push(AppliedChange {
                path: path.clone(),
                raw: entry.raw.clone(),
                change: ChangeKind::Deleted {
                    old: entry.initial.content().unwrap_or_default().to_string(),
                },
            });
        }
        Ok(Applied {
            changes: committed,
            summary: self.summary,
        })
    }

    fn write_change(path: &Path, entry: &VfsEntry) -> AppliedChange {
        let new = entry.current.content().unwrap().to_string();
        let old = entry.initial.content().map(str::to_string);
        let change = match old {
            Some(old) if !entry.added => ChangeKind::Updated { old, new },
            overwritten => ChangeKind::Added {
                content: new,
                overwritten,
            },
        };
        AppliedChange {
            path: path.to_path_buf(),
            raw: entry.raw.clone(),
            change,
        }
    }
}

fn read_state(path: &Path) -> io::Result<VfsState> {
    match fs::metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "path is a directory",
                ));
            }
            Ok(VfsState::Text {
                content: fs::read_to_string(path)?,
                mode: Some(metadata.permissions()),
                origin: path.to_path_buf(),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(VfsState::Absent),
        Err(error) => Err(error),
    }
}

fn require_text<'a>(
    state: &'a VfsState,
    path: &Path,
    context: &str,
) -> Result<&'a str, ApplyPatchError> {
    state
        .content()
        .ok_or_else(|| io_error(context, path, io::Error::from(io::ErrorKind::NotFound)))
}

fn io_error(context: &str, path: &Path, source: io::Error) -> ApplyPatchError {
    IoError {
        context: format!("{context} {}", path.display()),
        source,
    }
    .into()
}

fn write_atomically(
    path: &Path,
    content: &str,
    mode: Option<Permissions>,
) -> Result<(), ApplyPatchError> {
    // VFS keys already resolve content destinations. Persist replaces this
    // exact entry (including a link deleted then re-added during planning).
    let destination = path;
    match fs::symlink_metadata(destination) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("Failed to write file", path, error)),
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| io_error("Failed to create parent directories for", path, error))?;
    let write = || -> io::Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(content.as_bytes())?;
        temp.flush()?;
        if let Some(mode) = mode {
            temp.as_file().set_permissions(mode)?;
        }
        temp.persist(destination).map_err(|error| error.error)?;
        Ok(())
    };
    write().map_err(|error| io_error("Failed to write file", path, error))
}

/// Canonicalize existing parents, preserving missing suffixes for new files.
/// The last component is deliberately not followed: it may be unlinked.
fn entry_destination(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut missing = Vec::new();
    for ancestor in parent.ancestors() {
        match ancestor.canonicalize() {
            Ok(mut resolved) => {
                resolved.extend(missing.iter().rev());
                return Ok(resolved.join(path.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "path has no filename")
                })?));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(error);
                }
                missing.push(ancestor.file_name().ok_or(error)?.to_owned());
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::from(io::ErrorKind::NotFound))
}
