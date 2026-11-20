//! What can be seen of a running product from outside: `/proc` only.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Field 22 of `/proc/<pid>/stat`: with the pid, names one process
    /// incarnation, so a reused pid is never mistaken for the original.
    pub start_ticks: u64,
    pub state: char,
    pub exe: Option<PathBuf>,
}

fn parse_stat(pid: i32, stat: &str) -> Option<ProcInfo> {
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(ProcInfo {
        pid,
        state: fields.first()?.chars().next()?,
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
        exe: std::fs::read_link(format!("/proc/{pid}/exe")).ok(),
    })
}

pub fn process(pid: i32) -> Option<ProcInfo> {
    parse_stat(
        pid,
        &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?,
    )
}

pub fn all_processes() -> Vec<ProcInfo> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            process(pid)
        })
        .collect()
}

/// Every descendant of `root` by parent links (not including `root`),
/// whatever process group it moved to.
pub fn descendants(root: i32) -> Vec<ProcInfo> {
    let all = all_processes();
    let mut found: BTreeSet<i32> = BTreeSet::from([root]);
    loop {
        let before = found.len();
        for p in &all {
            if found.contains(&p.ppid) {
                found.insert(p.pid);
            }
        }
        if found.len() == before {
            break;
        }
    }
    all.into_iter()
        .filter(|p| p.pid != root && found.contains(&p.pid))
        .collect()
}

/// A process is gone only when its pid no longer names the same incarnation
/// and it is not a zombie either.
pub fn is_gone(p: &ProcInfo) -> bool {
    match process(p.pid) {
        Some(now) => now.start_ticks != p.start_ticks,
        None => true,
    }
}

pub fn cmdline(pid: i32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdInfo {
    pub fd: i32,
    /// `readlink` of the descriptor: a path, `socket:[n]`, `pipe:[n]`,
    /// `anon_inode:...`.
    pub target: String,
    pub cloexec: bool,
}
impl FdInfo {
    pub fn kind(&self) -> &'static str {
        match self.target.as_str() {
            t if t.starts_with("socket:") => "socket",
            t if t.starts_with("pipe:") => "pipe",
            t if t.starts_with("anon_inode:") => "anon_inode",
            "/dev/null" => "null",
            t if t.starts_with('/') => "file",
            _ => "other",
        }
    }
}

/// Open descriptors of a process, with their close-on-exec flag. The listing
/// itself opens one descriptor; it is dropped from the result.
pub fn open_fds(pid: i32) -> std::io::Result<Vec<FdInfo>> {
    let dir = format!("/proc/{pid}/fd");
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue; // closed meanwhile, e.g. the directory's own descriptor
        };
        let flags = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("flags:"))
                    .and_then(|v| i64::from_str_radix(v.trim(), 8).ok())
            })
            .unwrap_or(0);
        fds.push(FdInfo {
            fd,
            target: target.to_string_lossy().into_owned(),
            cloexec: flags & 0o2000000 != 0,
        });
    }
    fds.sort_by_key(|f| f.fd);
    Ok(fds)
}

/// Same file, by device and inode (never by path text).
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}
