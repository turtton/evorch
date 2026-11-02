//! Reading the files an evaluation leaves behind without trusting them: every
//! path is confined to the evaluation root, nothing is followed through a
//! symbolic link, and every read is bounded. A file that is absent, unreadable,
//! too large or malformed is reported as such rather than as one failure of the
//! whole snapshot, because the writers replace these files while a reader looks.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
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

fn from_errno<T>(path: &Path, errno: Errno) -> Artifact<T> {
    match errno {
        Errno::NOENT | Errno::NOTDIR => Artifact::Missing,
        Errno::LOOP => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            path,
            "is a symbolic link",
        )),
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

/// Open a regular file for reading. A symbolic link at the last component is
/// refused by the kernel, and a FIFO or device cannot block the open.
fn open_regular(path: &Path) -> Artifact<(File, u64)> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let descriptor = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(descriptor) => descriptor,
        Err(errno) => return from_errno(path, errno),
    };
    let file = File::from(descriptor);
    match file.metadata() {
        Ok(meta) if meta.is_file() => Artifact::Present((file, meta.len())),
        Ok(_) => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            path,
            "is not a regular file",
        )),
        Err(error) => Artifact::Problem(ArtifactProblem::io(path, &error)),
    }
}

pub(super) fn read_bytes(path: &Path, limit: u64) -> Artifact<Vec<u8>> {
    open_regular(path).and_then(|(file, length)| {
        let too_large = |length: u64| {
            Artifact::Problem(ArtifactProblem::new(
                ProblemKind::TooLarge,
                path,
                format!("{length} bytes, over the {limit} byte limit"),
            ))
        };
        if length > limit {
            return too_large(length);
        }
        let mut bytes = Vec::new();
        // The file may grow after the size check; the take keeps the read bounded.
        if let Err(error) = file.take(limit.saturating_add(1)).read_to_end(&mut bytes) {
            return Artifact::Problem(ArtifactProblem::io(path, &error));
        }
        if bytes.len() as u64 > limit {
            return too_large(bytes.len() as u64);
        }
        Artifact::Present(bytes)
    })
}

pub(super) fn read_json<T: DeserializeOwned>(path: &Path, limit: u64) -> Artifact<T> {
    read_bytes(path, limit).and_then(|bytes| match serde_json::from_slice(&bytes) {
        Ok(value) => Artifact::Present(value),
        Err(error) => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::Malformed,
            path,
            error.to_string(),
        )),
    })
}

/// Parse JSON that carries a `schema` number, refusing other versions before
/// trying to understand the rest.
pub(super) fn read_versioned_json<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    supported: u64,
) -> Artifact<T> {
    read_json::<serde_json::Value>(path, limit).and_then(|value| {
        if let Some(found) = value.get("schema")
            && found.as_u64() != Some(supported)
        {
            return Artifact::Problem(ArtifactProblem::new(
                ProblemKind::UnsupportedSchema,
                path,
                format!("schema {found}, this reader understands {supported}"),
            ));
        }
        match serde_json::from_value(value) {
            Ok(value) => Artifact::Present(value),
            Err(error) => Artifact::Problem(ArtifactProblem::new(
                ProblemKind::Malformed,
                path,
                error.to_string(),
            )),
        }
    })
}

/// At most `max_bytes` from the start of a text file.
pub(super) fn read_text_head(path: &Path, max_bytes: u64) -> Artifact<BoundedText> {
    open_regular(path).and_then(|(file, total_bytes)| {
        let mut bytes = Vec::new();
        if let Err(error) = file.take(max_bytes).read_to_end(&mut bytes) {
            return Artifact::Problem(ArtifactProblem::io(path, &error));
        }
        let truncated = total_bytes > bytes.len() as u64;
        Artifact::Present(BoundedText {
            text: decode(&bytes, truncated),
            total_bytes,
            truncated,
        })
    })
}

/// Up to `max_bytes` from the end of a text file, starting at a character boundary.
pub(super) fn read_text_tail(path: &Path, max_bytes: u64) -> Artifact<String> {
    open_regular(path).and_then(|(mut file, length)| {
        let start = length.saturating_sub(max_bytes);
        let mut bytes = Vec::new();
        let read = file
            .seek(SeekFrom::Start(start))
            .and_then(|_| file.take(max_bytes).read_to_end(&mut bytes));
        if let Err(error) = read {
            return Artifact::Problem(ArtifactProblem::io(path, &error));
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

fn decode(bytes: &[u8], cut_at_end: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(error) if cut_at_end && error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Describe a file without opening its content.
pub(super) fn file_ref(path: PathBuf) -> FileRef {
    let state = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => FileState::Present { bytes: meta.len() },
        Ok(_) => FileState::Rejected(ProblemKind::UnsafePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            FileState::Rejected(ProblemKind::PermissionDenied)
        }
        Err(_) => FileState::Rejected(ProblemKind::Io),
    };
    FileRef { path, state }
}

/// A single plain path component: no separators, no `.` or `..`.
pub(super) fn is_plain_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(only)), None) if only == name
    )
}

/// The directory `name` directly inside `parent`, which must itself be
/// canonical. A symbolic link or anything that resolves elsewhere is refused.
pub(super) fn child_dir(parent: &Path, name: &str) -> Artifact<PathBuf> {
    let path = parent.join(name);
    if !is_plain_name(name) {
        return Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            &path,
            format!("{name:?} is not a plain directory name"),
        ));
    }
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(meta) => {
            let what = if meta.file_type().is_symlink() {
                "is a symbolic link"
            } else {
                "is not a directory"
            };
            return Artifact::Problem(ArtifactProblem::new(ProblemKind::UnsafePath, &path, what));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Artifact::Missing,
        Err(error) => return Artifact::Problem(ArtifactProblem::io(&path, &error)),
    }
    match path.canonicalize() {
        Ok(resolved) if resolved.parent() == Some(parent) => Artifact::Present(resolved),
        Ok(resolved) => Artifact::Problem(ArtifactProblem::new(
            ProblemKind::UnsafePath,
            &path,
            format!("resolves to {}", resolved.display()),
        )),
        Err(error) => Artifact::Problem(ArtifactProblem::io(&path, &error)),
    }
}
