//! Opt-in scratch root format. Unmarked roots are never adopted by the collector.
use super::{cleanup, euid};
use rustix::fs::{
    AtFlags, FileType, FlockOperation, Mode, OFlags, Stat, flock, fstat, open, openat, statat,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read, Write},
    os::{
        fd::OwnedFd,
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub(super) const MARKER: &str = ".evorch-scratch.json";
// A PATH_MAX-sized Unix path can use four JSON bytes per non-UTF-8 byte.
pub(super) const LIMIT: u64 = 32 * 1024;
pub(super) const TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(super) const FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Fixed,
    Ephemeral,
    Standalone,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Format {
    version: u8,
    kind: Kind,
    owner: Option<Vec<u8>>,
}
impl Format {
    pub(super) fn new(kind: Kind, owner: Option<&Path>) -> Self {
        Self {
            version: 1,
            kind,
            owner: owner.map(|p| p.as_os_str().as_bytes().to_vec()),
        }
    }
    pub(super) fn write(&self, fd: &OwnedFd) -> io::Result<()> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid());
        }
        let marker = openat(
            fd,
            MARKER,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        fs::File::from(marker).write_all(&bytes)
    }
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "invalid or replaced managed scratch root",
    )
}
pub(super) fn private(s: &Stat) -> bool {
    s.st_uid == euid()
        && s.st_mode & 0o077 == 0
        && FileType::from_raw_mode(s.st_mode) == FileType::Directory
}
pub(super) fn same(a: &Stat, b: &Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
#[derive(Debug)]
pub(super) struct Lease {
    fd: OwnedFd,
}
impl Lease {
    pub(super) fn acquire(path: &Path) -> io::Result<Self> {
        let fd = open(path, FLAGS, Mode::empty())?;
        flock(&fd, FlockOperation::NonBlockingLockShared)?;
        let lease = Self { fd };
        lease.validate(path)?;
        Ok(lease)
    }
    pub(super) fn validate(&self, path: &Path) -> io::Result<()> {
        let s = fstat(&self.fd)?;
        let current = rustix::fs::statat(rustix::fs::CWD, path, AtFlags::SYMLINK_NOFOLLOW)?;
        if !private(&s) || !same(&s, &current) {
            return Err(invalid());
        }
        Ok(())
    }
    pub(super) fn fd(&self) -> &OwnedFd {
        &self.fd
    }
}
fn read(fd: &OwnedFd) -> io::Result<(Stat, Vec<u8>, Format, SystemTime)> {
    let file = openat(
        fd,
        MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let s = fstat(&file)?;
    if s.st_uid != euid()
        || s.st_mode & 0o077 != 0
        || s.st_nlink != 1
        || s.st_size < 0
        || s.st_size as u64 > LIMIT
        || FileType::from_raw_mode(s.st_mode) != FileType::RegularFile
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    let file = fs::File::from(file);
    let modified = file.metadata()?.modified()?;
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid());
    }
    let format: Format = serde_json::from_slice(&bytes)?;
    if format.version != 1 {
        return Err(invalid());
    }
    Ok((s, bytes, format, modified))
}
fn kind(name: &[u8]) -> Option<Kind> {
    let suffix = name.strip_prefix(format!("evo-{}-", euid()).as_bytes());
    if let Some(s) = suffix {
        if s.len() == 8
            && s.iter()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return Some(Kind::Fixed);
        }
        if s.len() == 6 && s.iter().all(u8::is_ascii_alphanumeric) {
            return Some(Kind::Ephemeral);
        }
    }
    let s = name.strip_prefix(b"evo-")?;
    (s.len() == 6 && s.iter().all(u8::is_ascii_alphanumeric)).then_some(Kind::Standalone)
}
fn owner_absent(format: &Format, name: &[u8], fd: &OwnedFd) -> io::Result<bool> {
    if format.kind != Kind::Fixed {
        return Ok(format.owner.is_none());
    }
    let Some(owner) = &format.owner else {
        return Ok(false);
    };
    let path = PathBuf::from(std::ffi::OsString::from_vec(owner.clone()));
    if !path.is_absolute() {
        return Ok(false);
    }
    let digest: String = Sha256::digest(owner)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if name != format!("evo-{}-{}", euid(), &digest[..8]).as_bytes() {
        return Ok(false);
    }
    let marker = openat(
        fd,
        super::IDENTITY_MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    if super::read_marker_fd(marker)? != digest {
        return Ok(false);
    }
    // Only a definitive ENOENT qualifies; permission/mount lookup errors retain it.
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => Err(e),
    }
}
pub(super) struct Candidate {
    pub fd: OwnedFd,
    root: Stat,
    marker: Stat,
    bytes: Vec<u8>,
}
impl Candidate {
    pub(super) fn open(
        parent: &OwnedFd,
        name: &std::ffi::CStr,
        now: SystemTime,
    ) -> io::Result<Option<Self>> {
        let Some(expected) = kind(name.to_bytes()) else {
            return Ok(None);
        };
        let fd = openat(parent, name, FLAGS, Mode::empty())?;
        let root = fstat(&fd)?;
        if !private(&root) {
            return Ok(None);
        }
        match flock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let (marker, bytes, format, modified) = read(&fd)?;
        if format.kind != expected
            || !now.duration_since(modified).is_ok_and(|age| age > TTL)
            || !owner_absent(&format, name.to_bytes(), &fd)?
        {
            return Ok(None);
        }
        let candidate = Self {
            fd,
            root,
            marker,
            bytes,
        };
        candidate.validate(parent, name)?;
        Ok(Some(candidate))
    }
    pub(super) fn inode(&self) -> cleanup::Identity {
        cleanup::identity(&self.root)
    }
    pub(super) fn validate(&self, parent: &OwnedFd, name: &std::ffi::CStr) -> io::Result<()> {
        let current = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        let (marker, bytes, format, _) = read(&self.fd)?;
        if !same(&current, &self.root)
            || !private(&current)
            || !same(&marker, &self.marker)
            || bytes != self.bytes
            || marker.st_mtime != self.marker.st_mtime
            || marker.st_mtime_nsec != self.marker.st_mtime_nsec
            || !owner_absent(&format, name.to_bytes(), &self.fd)?
        {
            return Err(invalid());
        }
        Ok(())
    }
}
