//! FD-relative workspace reclamation. Only the two fixed workspace locations
//! are candidates; manifest paths are evidence, never deletion instructions.
use std::ffi::{CStr, OsString};
use std::fs::{File, TryLockError};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rustix::fd::OwnedFd;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::output::{MANIFEST_FILE, Manifest};

pub const LOCK_FILE: &str = ".workspace.lock";
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const REGULAR: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    pub dry_run: bool,
    pub include_unfinished: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    WouldRemove,
    Removed,
    Skip(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Size {
    Bytes(u64),
    Unknown(String),
    NotMeasured,
}

#[derive(Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub action: Action,
    pub size: Size,
}

#[derive(Debug, Default)]
pub struct Report {
    pub entries: Vec<Entry>,
    /// Legacy unfinished evaluations accepted explicitly by the caller.
    pub unfinished_legacy: Vec<PathBuf>,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.action, Action::Failed(_)))
    }

    fn problem(&mut self, path: PathBuf, problem: Problem) {
        let size = Size::Unknown(problem.reason.clone());
        let action = if problem.failed {
            Action::Failed(problem.reason)
        } else {
            Action::Skip(problem.reason)
        };
        self.entries.push(Entry { path, action, size });
    }
}

struct Problem {
    reason: String,
    failed: bool,
    missing: bool,
}
impl Problem {
    fn skip(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            failed: false,
            missing: false,
        }
    }
}
impl From<io::Error> for Problem {
    fn from(error: io::Error) -> Self {
        Self {
            reason: error.to_string(),
            failed: true,
            missing: error.kind() == io::ErrorKind::NotFound,
        }
    }
}
impl From<Errno> for Problem {
    fn from(error: Errno) -> Self {
        io::Error::from(error).into()
    }
}
type Result<T> = std::result::Result<T, Problem>;

struct Dir {
    fd: OwnedFd,
    path: PathBuf,
    stat: Stat,
}
impl Dir {
    // Only caller-specified roots may follow links (e.g. macOS /var).
    fn root(path: &Path) -> Result<Self> {
        let fd = rustix::fs::open(path, DIRECTORY - OFlags::NOFOLLOW, Mode::empty())?;
        let stat = rustix::fs::fstat(&fd)?;
        Ok(Self {
            fd,
            path: path.into(),
            stat,
        })
    }

    fn child(&self, name: &CStr) -> Result<Option<Self>> {
        let Some(stat) = self.stat(name)? else {
            return Ok(None);
        };
        if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
            return Err(Problem::skip("symlink"));
        }
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(Problem::skip("not-directory"));
        }
        self.same_device(&stat)?;
        let fd = match rustix::fs::openat(&self.fd, name, DIRECTORY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let opened = rustix::fs::fstat(&fd)?;
        if !same(&stat, &opened) {
            return Err(io::Error::other("directory changed while opening").into());
        }
        Ok(Some(Self {
            fd,
            path: self.path.join(os_name(name)),
            stat: opened,
        }))
    }

    fn same_device(&self, stat: &Stat) -> Result<()> {
        if stat.st_dev != self.stat.st_dev {
            return Err(io::Error::other("different filesystem; refusing to descend").into());
        }
        Ok(())
    }

    fn stat(&self, name: &CStr) -> Result<Option<Stat>> {
        match rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(stat)),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    // Raw names, no UTF-8 filtering or entry count limit.
    fn names(&self) -> Result<Vec<std::ffi::CString>> {
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.fd)? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    fn regular(&self, name: &CStr, create: bool) -> Result<Option<File>> {
        let flags = REGULAR
            | if create {
                OFlags::CREATE | OFlags::RDWR
            } else {
                OFlags::empty()
            };
        let fd = match rustix::fs::openat(&self.fd, name, flags, Mode::RUSR | Mode::WUSR) {
            Ok(fd) => fd,
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(Errno::LOOP) => return Err(Problem::skip("symlink")),
            Err(error) => return Err(error.into()),
        };
        let stat = rustix::fs::fstat(&fd)?;
        self.same_device(&stat)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::other("not a regular file").into());
        }
        Ok(Some(File::from(fd)))
    }

    fn manifest(&self) -> Result<Manifest> {
        const LIMIT: u64 = 16 * 1024 * 1024;
        let file = self
            .regular(c"manifest.json", false)?
            .ok_or_else(|| Problem::skip("not-evaluation: missing manifest.json"))?;
        let mut bytes = Vec::new();
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(Problem::skip("manifest exceeds 16 MiB"));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| Problem::skip(format!("invalid {MANIFEST_FILE}: {error}")))
    }
}

fn os_name(name: &CStr) -> OsString {
    OsString::from_vec(name.to_bytes().to_vec())
}
fn same(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

/// Holds the runner's exclusive, close-on-exec lock until explicit cleanup has
/// finished. Drop only releases descriptors; it never deletes anything.
pub(crate) struct RunnerGuard {
    root: Dir,
    _lock: File,
}
impl RunnerGuard {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let acquire = || -> Result<Self> {
            let root = Dir::root(path)?;
            let lock = root
                .regular(c".workspace.lock", true)?
                .expect("create returns a file");
            lock.lock()?;
            // Recheck under the lock: another runner may have won the race
            // between create_dir's empty check and opening this descriptor.
            if root
                .names()?
                .iter()
                .any(|name| name.as_c_str() != c".workspace.lock")
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "output directory is not empty",
                )
                .into());
            }
            Ok(Self { root, _lock: lock })
        };
        acquire().map_err(|problem| io::Error::other(problem.reason))
    }

    pub(crate) fn cleanup(&self) -> Report {
        let mut report = Report::default();
        workspaces(&self.root, false, false, &mut report);
        report
    }
}

/// Clean explicit evaluation roots, or discover only immediate children of
/// the trusted eval home. This function performs no config/provider access.
pub fn clean(outputs: &[PathBuf], eval_home: Option<&Path>, options: Options) -> Report {
    let mut report = Report::default();
    for path in outputs {
        match Dir::root(path) {
            Ok(root) => evaluation(&root, options, &mut report),
            Err(problem) if problem.missing => {}
            Err(problem) => report.problem(path.clone(), problem),
        }
    }
    if outputs.is_empty()
        && let Some(path) = eval_home
    {
        match Dir::root(path) {
            Ok(home) => match home.names() {
                Ok(names) => {
                    for name in names {
                        let path = home.path.join(os_name(&name));
                        match home.child(&name) {
                            Ok(Some(root)) => evaluation(&root, options, &mut report),
                            Ok(None) => {}
                            Err(problem) => report.problem(path, problem),
                        }
                    }
                }
                Err(problem) => report.problem(path.into(), problem),
            },
            Err(problem) => {
                // A missing default eval home simply has no evaluations.
                if !problem.missing {
                    report.problem(path.into(), problem);
                }
            }
        }
    }
    report
}

fn evaluation(root: &Dir, options: Options, report: &mut Report) {
    let check = || -> Result<(Option<File>, bool)> {
        let lock = root.regular(c".workspace.lock", false)?;
        if let Some(lock) = &lock {
            match lock.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => return Err(Problem::skip("busy")),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let manifest = root.manifest()?;
        let legacy = lock.is_none() && manifest.finished_at.is_none();
        if legacy && !options.include_unfinished {
            return Err(Problem::skip("unfinished-legacy"));
        }
        Ok((lock, legacy))
    };
    match check() {
        Ok((_lock, legacy)) => {
            if legacy {
                report.unfinished_legacy.push(root.path.clone());
            }
            workspaces(root, options.dry_run, true, report);
        }
        Err(problem) => report.problem(root.path.clone(), problem),
    }
}

fn workspaces(root: &Dir, dry_run: bool, measure: bool, report: &mut Report) {
    workspace(root, c"preflight-workspace", dry_run, measure, report);
    let runs = match root.child(c"runs") {
        Ok(Some(runs)) => runs,
        Ok(None) => return,
        Err(problem) => {
            report.problem(root.path.join("runs"), problem);
            return;
        }
    };
    let names = match runs.names() {
        Ok(names) => names,
        Err(problem) => {
            report.problem(runs.path.clone(), problem);
            return;
        }
    };
    for name in names {
        match runs.child(&name) {
            Ok(Some(run)) => workspace(&run, c"workspace", dry_run, measure, report),
            Ok(None) => {}
            Err(problem) => report.problem(runs.path.join(os_name(&name)), problem),
        }
    }
}

fn workspace(parent: &Dir, name: &CStr, dry_run: bool, measure: bool, report: &mut Report) {
    let path = parent.path.join(os_name(name));
    let result = (|| -> Result<Option<u64>> {
        let Some(dir) = parent.child(name)? else {
            return Ok(None);
        };
        sync_point(&path);
        let bytes = walk(&dir, dry_run, measure, 0)?;
        if !dry_run {
            remove(parent, name, &dir.stat, AtFlags::REMOVEDIR)?;
        }
        Ok(Some(bytes))
    })();
    match result {
        Ok(Some(bytes)) => report.entries.push(Entry {
            path,
            action: if dry_run {
                Action::WouldRemove
            } else {
                Action::Removed
            },
            size: if measure {
                Size::Bytes(bytes)
            } else {
                Size::NotMeasured
            },
        }),
        Ok(None) => {}
        Err(problem) => report.problem(path, problem),
    }
}

fn walk(dir: &Dir, dry_run: bool, measure: bool, depth: usize) -> Result<u64> {
    if depth >= 256 {
        return Err(io::Error::other("directory nesting exceeds safe traversal depth").into());
    }
    let mut bytes = 0_u64;
    let mut failure = None;
    for name in dir.names()? {
        let result = (|| -> Result<u64> {
            let Some(stat) = dir.stat(&name)? else {
                return Ok(0);
            };
            dir.same_device(&stat)?;
            let kind = FileType::from_raw_mode(stat.st_mode);
            let size = if kind == FileType::Directory {
                let Some(child) = dir.child(&name)? else {
                    return Ok(0);
                };
                let size = walk(&child, dry_run, measure, depth + 1)?;
                if !dry_run {
                    remove(dir, &name, &child.stat, AtFlags::REMOVEDIR)?;
                }
                size
            } else {
                let size = if measure && matches!(kind, FileType::RegularFile | FileType::Symlink) {
                    u64::try_from(stat.st_size)
                        .map_err(|_| io::Error::other("negative logical size"))?
                } else {
                    0
                };
                if !dry_run {
                    remove(dir, &name, &stat, AtFlags::empty())?;
                }
                size
            };
            Ok(size)
        })();
        match result {
            Ok(size) => match bytes.checked_add(size) {
                Some(total) => bytes = total,
                None => failure = Some(io::Error::other("logical size overflow").into()),
            },
            Err(problem) => failure = Some(problem),
        }
    }
    if let Some(problem) = failure {
        Err(problem)
    } else {
        Ok(bytes)
    }
}

fn remove(parent: &Dir, name: &CStr, expected: &Stat, flags: AtFlags) -> Result<()> {
    let Some(current) = parent.stat(name)? else {
        return Ok(());
    };
    if !same(&current, expected) {
        return Err(io::Error::other("entry changed during cleanup").into());
    }
    match rustix::fs::unlinkat(&parent.fd, name, flags) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(test))]
fn sync_point(_: &Path) {}
#[cfg(test)]
type Hook = Box<dyn FnOnce(&Path)>;
#[cfg(test)]
thread_local! { static HOOK: std::cell::RefCell<Option<Hook>> = Default::default(); }
#[cfg(test)]
pub(crate) fn on_workspace_opened(hook: impl FnOnce(&Path) + 'static) {
    HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}
#[cfg(test)]
fn sync_point(path: &Path) {
    HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook(path);
        }
    });
}
#[cfg(test)]
mod tests;
