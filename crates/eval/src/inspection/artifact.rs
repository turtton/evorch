//! Reading the files an evaluation leaves behind without trusting them: every
//! path is confined to the evaluation root, nothing is followed through a
//! symbolic link, and every read is bounded. A file that is absent, unreadable,
//! too large or malformed is reported as such rather than as one failure of the
//! whole snapshot, because the writers replace these files while a reader looks.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use serde::de::DeserializeOwned;

/// Why a file or directory could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    /// The content is not what the reader understands.
    Malformed,
    /// A newer or unknown schema version.
    UnsupportedSchema,
    /// Larger than the reader's bound.
    TooLarge,
    PermissionDenied,
    /// A symbolic link, a path outside the evaluation root or a file of the
    /// wrong type.
    UnsafePath,
    Io,
}

impl ProblemKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::UnsupportedSchema => "unsupported_schema",
            Self::TooLarge => "too_large",
            Self::PermissionDenied => "permission_denied",
            Self::UnsafePath => "unsafe_path",
            Self::Io => "io",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactProblem {
    pub kind: ProblemKind,
    pub path: PathBuf,
    pub detail: String,
}

impl ArtifactProblem {
    pub fn new(kind: ProblemKind, path: &Path, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.to_path_buf(),
            detail: detail.into(),
        }
    }

    pub(super) fn io(path: &Path, error: &std::io::Error) -> Self {
        let kind = if error.kind() == std::io::ErrorKind::PermissionDenied {
            ProblemKind::PermissionDenied
        } else {
            ProblemKind::Io
        };
        Self::new(kind, path, error.to_string())
    }
}

impl fmt::Display for ArtifactProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} ({})",
            self.path.display(),
            self.detail,
            self.kind.as_str()
        )
    }
}

/// What reading one artifact found. `Missing` is expected while a file is not
/// yet written or was deleted; `Problem` says why an existing one is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Artifact<T> {
    Present(T),
    Missing,
    Problem(ArtifactProblem),
}

impl<T> Artifact<T> {
    pub const fn present(&self) -> Option<&T> {
        match self {
            Self::Present(value) => Some(value),
            _ => None,
        }
    }

    pub const fn problem(&self) -> Option<&ArtifactProblem> {
        match self {
            Self::Problem(problem) => Some(problem),
            _ => None,
        }
    }

    pub const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub fn map<U>(self, convert: impl FnOnce(T) -> U) -> Artifact<U> {
        match self {
            Self::Present(value) => Artifact::Present(convert(value)),
            Self::Missing => Artifact::Missing,
            Self::Problem(problem) => Artifact::Problem(problem),
        }
    }

    pub fn and_then<U>(self, convert: impl FnOnce(T) -> Artifact<U>) -> Artifact<U> {
        match self {
            Self::Present(value) => convert(value),
            Self::Missing => Artifact::Missing,
            Self::Problem(problem) => Artifact::Problem(problem),
        }
    }

    pub fn into_present(self) -> Option<T> {
        match self {
            Self::Present(value) => Some(value),
            _ => None,
        }
    }
}

/// The start of a text file, cut at a character boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedText {
    pub text: String,
    /// Size of the whole file in bytes.
    pub total_bytes: u64,
    pub truncated: bool,
}

/// Whether a file exists, without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    Present { bytes: u64 },
    Missing,
    Rejected(ProblemKind),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub path: PathBuf,
    pub state: FileState,
}

#[cfg(test)]
type SyncHook = Box<dyn Fn(&str, &Path)>;

#[cfg(test)]
thread_local! {
    static SYNC_HOOK: std::cell::RefCell<Option<SyncHook>> = const { std::cell::RefCell::new(None) };
}

/// Run the hook a test installed, between opening something and using it, to
/// replace paths at exactly that point.
#[cfg(test)]
pub(super) fn sync_point(name: &str, path: &Path) {
    SYNC_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(name, path);
        }
    });
}

#[cfg(not(test))]
#[inline]
pub(super) fn sync_point(_name: &str, _path: &Path) {}

#[cfg(test)]
pub(super) fn set_sync_hook(hook: Option<SyncHook>) {
    SYNC_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn from_errno<T>(path: &Path, errno: Errno, directory: bool) -> Artifact<T> {
    let unsafe_path = |detail: &str| {
        Artifact::Problem(ArtifactProblem::new(ProblemKind::UnsafePath, path, detail))
    };
    match errno {
        Errno::NOENT => Artifact::Missing,
        Errno::LOOP => unsafe_path("is a symbolic link"),
        Errno::NOTDIR if directory => unsafe_path("is not a directory or is a symbolic link"),
        Errno::NOTDIR => unsafe_path("has a component that is not a directory"),
        Errno::ACCESS | Errno::PERM => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::PermissionDenied,
            path,
            errno.to_string(),
        )),
        other => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::Io,
            path,
            other.to_string(),
        )),
    }
}

/// An open directory. Every name is resolved against this descriptor and
/// opened without following a link, so what was checked is what is read even
/// if the path is replaced afterwards.
pub(super) struct Dir {
    fd: OwnedFd,
    /// Where the directory was opened, for messages and for callers that show it.
    path: PathBuf,
}

impl Dir {
    /// Open the evaluation root. The path itself is trusted and may be a link.
    pub(super) fn open_root(path: &Path) -> Artifact<Self> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        match rustix::fs::open(path, flags, Mode::empty()) {
            Ok(fd) => Artifact::Present(Self {
                fd,
                path: path.to_path_buf(),
            }),
            Err(errno) => from_errno(path, errno, true),
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The directory `name` inside this one; a link, or a name with a separator
    /// or `..`, is refused.
    pub(super) fn open_dir(&self, name: &str) -> Artifact<Self> {
        let path = self.join(name);
        if !is_plain_name(name) {
            return Artifact::Problem(ArtifactProblem::new(
                ProblemKind::UnsafePath,
                &path,
                format!("{name:?} is not a plain directory name"),
            ));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match rustix::fs::openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => {
                sync_point("opened_dir", &path);
                Artifact::Present(Self { fd, path })
            }
            Err(errno) => from_errno(&path, errno, true),
        }
    }

    /// A regular file `name` inside this directory, never through a link, and
    /// never blocking on a FIFO or device.
    pub(super) fn open_regular(&self, name: &str) -> Artifact<(File, u64)> {
        let path = self.join(name);
        if !is_plain_name(name) {
            return Artifact::Problem(ArtifactProblem::new(
                ProblemKind::UnsafePath,
                &path,
                format!("{name:?} is not a plain file name"),
            ));
        }
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let descriptor = match rustix::fs::openat(&self.fd, name, flags, Mode::empty()) {
            Ok(descriptor) => descriptor,
            Err(errno) => return from_errno(&path, errno, false),
        };
        let file = File::from(descriptor);
        match file.metadata() {
            Ok(meta) if meta.is_file() => Artifact::Present((file, meta.len())),
            Ok(_) => Artifact::Problem(ArtifactProblem::new(
                ProblemKind::UnsafePath,
                &path,
                "is not a regular file",
            )),
            Err(error) => Artifact::Problem(ArtifactProblem::io(&path, &error)),
        }
    }

    pub(super) fn read_bytes(&self, name: &str, limit: u64) -> Artifact<Vec<u8>> {
        let path = self.join(name);
        self.open_regular(name).and_then(|(file, length)| {
            let too_large = |length: u64| {
                Artifact::Problem(ArtifactProblem::new(
                    ProblemKind::TooLarge,
                    &path,
                    format!("{length} bytes, over the {limit} byte limit"),
                ))
            };
            if length > limit {
                return too_large(length);
            }
            let mut bytes = Vec::new();
            // The file may grow after the size check; the take keeps the read bounded.
            if let Err(error) = file.take(limit.saturating_add(1)).read_to_end(&mut bytes) {
                return Artifact::Problem(ArtifactProblem::io(&path, &error));
            }
            if bytes.len() as u64 > limit {
                return too_large(bytes.len() as u64);
            }
            Artifact::Present(bytes)
        })
    }

    pub(super) fn read_json<T: DeserializeOwned>(&self, name: &str, limit: u64) -> Artifact<T> {
        let path = self.join(name);
        self.read_bytes(name, limit)
            .and_then(|bytes| match serde_json::from_slice(&bytes) {
                Ok(value) => Artifact::Present(value),
                Err(error) => Artifact::Problem(ArtifactProblem::new(
                    ProblemKind::Malformed,
                    &path,
                    error.to_string(),
                )),
            })
    }

    /// Parse JSON that carries a `schema` number, refusing other versions
    /// before trying to understand the rest.
    pub(super) fn read_versioned_json<T: DeserializeOwned>(
        &self,
        name: &str,
        limit: u64,
        supported: u64,
    ) -> Artifact<T> {
        let path = self.join(name);
        self.read_json::<serde_json::Value>(name, limit)
            .and_then(|value| {
                if let Some(found) = value.get("schema")
                    && found.as_u64() != Some(supported)
                {
                    return Artifact::Problem(ArtifactProblem::new(
                        ProblemKind::UnsupportedSchema,
                        &path,
                        format!("schema {found}, this reader understands {supported}"),
                    ));
                }
                match serde_json::from_value(value) {
                    Ok(value) => Artifact::Present(value),
                    Err(error) => Artifact::Problem(ArtifactProblem::new(
                        ProblemKind::Malformed,
                        &path,
                        error.to_string(),
                    )),
                }
            })
    }

    /// At most `max_bytes` from the start of a text file.
    pub(super) fn read_text_head(&self, name: &str, max_bytes: u64) -> Artifact<BoundedText> {
        let path = self.join(name);
        self.open_regular(name).and_then(|(file, total_bytes)| {
            let mut bytes = Vec::new();
            if let Err(error) = file.take(max_bytes).read_to_end(&mut bytes) {
                return Artifact::Problem(ArtifactProblem::io(&path, &error));
            }
            let truncated = total_bytes > bytes.len() as u64;
            Artifact::Present(BoundedText {
                text: decode(&bytes, truncated),
                total_bytes,
                truncated,
            })
        })
    }

    /// Up to `max_bytes` from the end of a text file, from a character boundary.
    pub(super) fn read_text_tail(&self, name: &str, max_bytes: u64) -> Artifact<String> {
        let path = self.join(name);
        self.open_regular(name).and_then(|(mut file, length)| {
            let start = length.saturating_sub(max_bytes);
            let mut bytes = Vec::new();
            let read = file
                .seek(SeekFrom::Start(start))
                .and_then(|_| file.take(max_bytes).read_to_end(&mut bytes));
            if let Err(error) = read {
                return Artifact::Problem(ArtifactProblem::io(&path, &error));
            }
            let text = String::from_utf8_lossy(&bytes);
            // A cut in the middle of a character leaves a replacement character first.
            let text = if start > 0 {
                text.strip_prefix('\u{FFFD}').unwrap_or(&text)
            } else {
                &text
            };
            Artifact::Present(text.to_owned())
        })
    }

    /// Describe a file without reading it.
    pub(super) fn file_ref(&self, name: &str) -> FileRef {
        let path = self.join(name);
        let state = if is_plain_name(name) {
            match rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                    FileState::Present {
                        bytes: u64::try_from(stat.st_size).unwrap_or(0),
                    }
                }
                Ok(_) => FileState::Rejected(ProblemKind::UnsafePath),
                Err(Errno::NOENT) => FileState::Missing,
                Err(Errno::ACCESS | Errno::PERM) => {
                    FileState::Rejected(ProblemKind::PermissionDenied)
                }
                Err(_) => FileState::Rejected(ProblemKind::Io),
            }
        } else {
            FileState::Rejected(ProblemKind::UnsafePath)
        };
        FileRef { path, state }
    }

    /// Entry names that are valid UTF-8, up to `max_entries` entries looked
    /// at, and whether more entries were left.
    pub(super) fn names(&self, max_entries: usize) -> Artifact<(Vec<String>, bool)> {
        let entries = match rustix::fs::Dir::read_from(&self.fd) {
            Ok(entries) => entries,
            Err(errno) => return from_errno(&self.path, errno, true),
        };
        let mut names = Vec::new();
        let mut looked_at = 0;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(errno) => return from_errno(&self.path, errno, true),
            };
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if looked_at == max_entries {
                return Artifact::Present((names, true));
            }
            looked_at += 1;
            if let Ok(name) = std::str::from_utf8(bytes) {
                names.push(name.to_owned());
            }
        }
        Artifact::Present((names, false))
    }
}

fn decode(bytes: &[u8], cut_at_end: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if cut_at_end && error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A single plain path component: no separators, no `.` or `..`.
pub(super) fn is_plain_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(only)), None) if only == name
    )
}
