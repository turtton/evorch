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
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                continue;
            };
            let f: Vec<_> = fields.split_whitespace().collect();
            let argv = std::fs::read(entry.path().join("cmdline"))
                .unwrap_or_default()
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            rows.push((
                pid,
                f[1].parse::<i32>().unwrap(),
                f[2].parse::<i32>().unwrap(),
                f[3].parse::<i32>().unwrap(),
                argv,
            ));
        }
        let mut descendants = HashSet::from([anchor]);
        loop {
            let before = descendants.len();
            for (pid, ppid, _, _, _) in &rows {
                if descendants.contains(ppid) {
                    descendants.insert(*pid);
                }
            }
            if before == descendants.len() {
                break;
            }
        }
        for (pid, ppid, pgid, sid, argv) in rows {
            if pid == anchor
                || !descendants.contains(&pid)
                || self.processes.iter().any(|p| p.pid == pid && !p.is_dead())
            {
                continue;
            }
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                continue;
            }
            self.processes.push(Process {
                pid,
                ppid,
                pgid,
                sid,
                argv,
                fd: unsafe { OwnedFd::from_raw_fd(fd as i32) },
            });
        }
    }
    pub fn trace(&self) {
        for p in &self.processes {
            println!(
                "B3_PROCESS pid={} ppid={} sid={} pgid={} argv={:?}",
                p.pid, p.ppid, p.sid, p.pgid, p.argv
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
