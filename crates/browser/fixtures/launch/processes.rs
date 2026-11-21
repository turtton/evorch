//! Linux-only outer controller. Its fixed 30s observation envelope is NOT a
//! product shutdown guarantee. External Chromium cannot acknowledge its death;
//! pidfd is the terminal signal, and poll's deadline only diagnoses a missing
//! signal (including mutations). No sleeps or kill(pid, 0). Every failure path
//! kills the pinned observed processes and reaps adopted children.
use std::{
    collections::HashSet,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};

pub struct Process {
    pub pid: i32,
    pub ppid: i32,
    pub sid: i32,
    pub pgid: i32,
    pub argv: Vec<String>,
    starttime: u64,
    fd: OwnedFd,
}
impl Process {
    pub fn signal(&self, signal: i32) {
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }
    }
    pub fn is_dead(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut fd, 1, 0) };
        assert!(result >= 0);
        result > 0
    }
}
pub fn subreaper() {
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
        0
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stat {
    pid: i32,
    ppid: i32,
    pgid: i32,
    sid: i32,
    starttime: u64,
}
impl Stat {
    fn read(pid: i32) -> Option<Self> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, fields) = stat.rsplit_once(") ")?;
        // The suffix starts at stat field 3 (state); starttime is field 22.
        let f: Vec<_> = fields.split_whitespace().collect();
        Some(Self {
            pid,
            ppid: f.get(1)?.parse().ok()?,
            pgid: f.get(2)?.parse().ok()?,
            sid: f.get(3)?.parse().ok()?,
            starttime: f.get(19)?.parse().ok()?,
        })
    }
}

fn pin_observed(
    observed: Stat,
    argv: Vec<String>,
    reread: impl FnOnce(i32) -> Option<Stat>,
) -> Option<Process> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, observed.pid, 0) };
    if fd < 0 {
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    // pidfd pins the signal target, but opening it can race with PID reuse.
    // Recheck identity AND ancestry/group metadata after opening it. An exit,
    // reparent, group change or reused PID discards the fd without any signal.
    if reread(observed.pid) != Some(observed) {
        return None;
    }
    Some(Process {
        pid: observed.pid,
        ppid: observed.ppid,
        pgid: observed.pgid,
        sid: observed.sid,
        starttime: observed.starttime,
        argv,
        fd,
    })
}

fn descendants(anchor: i32, links: &[(i32, i32)]) -> HashSet<i32> {
    let mut descendants = HashSet::from([anchor]);
    loop {
        let before = descendants.len();
        for (pid, ppid) in links {
            if descendants.contains(ppid) {
                descendants.insert(*pid);
            }
        }
        if before == descendants.len() {
            return descendants;
        }
    }
}

fn capture_observed(
    processes: &mut Vec<Process>,
    anchor: i32,
    rows: Vec<(Stat, Vec<String>)>,
    mut reread: impl FnMut(i32) -> Option<Stat>,
) {
    let raw_links: Vec<_> = rows.iter().map(|(s, _)| (s.pid, s.ppid)).collect();
    let candidates = descendants(anchor, &raw_links);
    let pinned: Vec<_> = rows
        .into_iter()
        .filter(|(s, _)| s.pid != anchor && candidates.contains(&s.pid))
        .filter_map(|(s, argv)| pin_observed(s, argv, &mut reread))
        .collect();
    // A rejected ancestor's stale row must not establish ownership of a child
    // whose own stat still matches. Use only pinned, revalidated rows here,
    // including already tracked live parents; deduplicate AFTER reachability.
    let valid_links: Vec<_> = pinned.iter().map(|p| (p.pid, p.ppid)).collect();
    let reachable = descendants(anchor, &valid_links);
    for p in pinned {
        if reachable.contains(&p.pid)
            && !processes
                .iter()
                .any(|tracked| tracked.pid == p.pid && !tracked.is_dead())
        {
            processes.push(p);
        }
    }
}

#[derive(Default)]
pub struct Tree {
    pub processes: Vec<Process>,
}
impl Tree {
    pub fn capture(&mut self, anchor: i32) {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let Some(stat) = Stat::read(pid) else {
                continue;
            };
            let argv = std::fs::read(entry.path().join("cmdline"))
                .unwrap_or_default()
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            rows.push((stat, argv));
        }
        capture_observed(&mut self.processes, anchor, rows, Stat::read);
    }
    pub fn trace(&self) {
        for p in &self.processes {
            println!(
                "B3_PROCESS pid={} ppid={} sid={} pgid={} starttime={} argv={:?}",
                p.pid, p.ppid, p.sid, p.pgid, p.starttime, p.argv
            );
        }
    }
    pub fn assert_group_dead(&self, pgid: i32) {
        let group: Vec<_> = self.processes.iter().filter(|p| p.pgid == pgid).collect();
        assert!(
            !group.is_empty(),
            "must observe the root, not pass vacuously"
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        for p in group {
            while !p.is_dead() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut fd = libc::pollfd {
                    fd: p.fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let result = unsafe {
                    libc::poll(
                        &mut fd,
                        1,
                        remaining.as_millis().min(i32::MAX as u128) as i32,
                    )
                };
                if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                assert!(
                    result > 0,
                    "same-PGID process still alive after owner termination: pid={} pgid={pgid}",
                    p.pid
                );
            }
        }
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        // Capture also children adopted after the first snapshot. This controller
        // is a dedicated test subprocess, so it owns every descendant here.
        self.capture(std::process::id() as i32);
        for p in &self.processes {
            p.signal(libc::SIGKILL);
        }
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
        }
    }
}

#[test]
fn changed_identity_after_pidfd_open_is_not_registered() {
    let current = Stat::read(std::process::id() as i32).unwrap();
    for change in ["starttime", "ppid", "pgid", "sid", "gone"] {
        let mut observed = current;
        match change {
            "starttime" => observed.starttime += 1,
            "ppid" => observed.ppid += 1,
            "pgid" => observed.pgid += 1,
            "sid" => observed.sid += 1,
            "gone" => {}
            _ => unreachable!(),
        }
        // Use a real pidfd, then deterministically model a stale /proc row.
        // Process only closes its fd, so even a broken guard cannot signal
        // this test process (unlike the outer controller's Tree destructor).
        let mut read_after_open = false;
        let pinned = pin_observed(observed, Vec::new(), |pid| {
            read_after_open = true;
            if change == "gone" {
                None
            } else {
                Stat::read(pid)
            }
        });
        assert!(read_after_open, "must reach the post-pidfd identity check");
        assert!(pinned.is_none(), "accepted a mismatched {change}");
    }
}

#[test]
fn unchanged_identity_after_pidfd_open_is_registered() {
    let current = Stat::read(std::process::id() as i32).unwrap();
    let pinned = pin_observed(current, Vec::new(), Stat::read).unwrap();
    assert_eq!(pinned.pid, current.pid);
    assert_eq!(pinned.starttime, current.starttime);
    assert!(!pinned.is_dead());
}

#[test]
fn matching_child_of_rejected_ancestor_is_not_registered() {
    let child = Stat::read(std::process::id() as i32).unwrap();
    let mut parent = Stat::read(child.ppid).unwrap();
    let anchor = parent.ppid;
    parent.starttime += 1;
    let mut registered = Vec::new();
    let mut child_validated = false;
    // Both are live real processes, but the ancestor's observed generation is
    // stale. A plain Vec closes fds without signalling even under mutation.
    capture_observed(
        &mut registered,
        anchor,
        vec![(parent, Vec::new()), (child, Vec::new())],
        |pid| {
            let actual = Stat::read(pid);
            if pid == child.pid {
                assert_eq!(actual, Some(child));
                child_validated = true;
            }
            actual
        },
    );
    assert!(child_validated, "must validate the matching child's pidfd");
    assert!(
        registered.is_empty(),
        "a rejected ancestor's stale edge registered an unrelated child"
    );
}
