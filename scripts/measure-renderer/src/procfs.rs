//! Process tree, memory and CPU readings from `/proc` (Linux only).
//!
//! RSS counts the shared libcef pages once per process; PSS divides shared
//! pages among the processes that map them; USS is the private part
//! (`Private_Clean + Private_Dirty`). PSS is the number to add up.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

/// `_SC_CLK_TCK` is 100 on every Linux the measurement is run on.
pub const CLK_TCK: f64 = 100.0;

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgrp: i32,
    pub state: char,
    /// `utime + stime` in clock ticks.
    pub ticks: u64,
}

fn parse_stat(pid: i32, text: &str) -> Option<Proc> {
    // `pid (comm) state ppid pgrp ...`; comm may hold spaces and parentheses.
    let rest = text.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let utime: u64 = fields.nth(8)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(Proc {
        pid,
        ppid,
        pgrp,
        state,
        ticks: utime + stime,
    })
}

pub fn read_proc(pid: i32) -> Option<Proc> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &text)
}

pub fn scan_all() -> Vec<Proc> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some(proc) = read_proc(pid) {
            found.push(proc);
        }
    }
    found
}

/// Live (non-zombie) processes that are `root` itself, in process group
/// `group`, or descend from either.
pub fn tree(root: i32, group: i32) -> Vec<Proc> {
    let all = scan_all();
    let mut members: HashSet<i32> = all
        .iter()
        .filter(|p| p.pid == root || p.pgrp == group)
        .map(|p| p.pid)
        .collect();
    loop {
        let before = members.len();
        for p in &all {
            if members.contains(&p.ppid) {
                members.insert(p.pid);
            }
        }
        if members.len() == before {
            break;
        }
    }
    all.into_iter()
        .filter(|p| members.contains(&p.pid) && p.state != 'Z')
        .collect()
}

/// Live children of this process (reparented orphans included, because the
/// measurement process is a child subreaper).
pub fn children_of_self() -> Vec<i32> {
    let me = std::process::id() as i32;
    scan_all()
        .into_iter()
        .filter(|p| p.ppid == me && p.state != 'Z')
        .map(|p| p.pid)
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Mem {
    pub rss_kib: u64,
    pub pss_kib: u64,
    pub uss_kib: u64,
}

pub fn mem(pid: i32) -> Option<Mem> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let mut mem = Mem::default();
    let mut seen = false;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok());
        let Some(value) = value else { continue };
        match key {
            "Rss" => {
                mem.rss_kib = value;
                seen = true;
            }
            "Pss" => mem.pss_kib = value,
            "Private_Clean" | "Private_Dirty" => mem.uss_kib += value,
            _ => {}
        }
    }
    seen.then_some(mem)
}

fn cmdline(pid: i32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `--type=<x>` of a Chromium process, `helper` for the renderer itself.
fn role(pid: i32, root: i32) -> String {
    if pid == root {
        return "helper".into();
    }
    cmdline(pid)
        .iter()
        .find_map(|arg| arg.strip_prefix("--type=").map(str::to_owned))
        .unwrap_or_else(|| "other".into())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RoleTotal {
    pub processes: usize,
    pub mem: Mem,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub processes: usize,
    pub mem: Mem,
    /// Processes whose `smaps_rollup` could not be read.
    pub unreadable: usize,
    pub roles: BTreeMap<String, RoleTotal>,
    /// Clock ticks per pid at the time of the snapshot.
    pub ticks: HashMap<i32, u64>,
}

/// Memory by role and CPU ticks of the helper and everything below it.
pub fn snapshot(root: i32, group: i32) -> Snapshot {
    let mut snap = Snapshot::default();
    for proc in tree(root, group) {
        snap.processes += 1;
        snap.ticks.insert(proc.pid, proc.ticks);
        let role = snap.roles.entry(role(proc.pid, root)).or_default();
        role.processes += 1;
        match mem(proc.pid) {
            Some(m) => {
                role.mem.rss_kib += m.rss_kib;
                role.mem.pss_kib += m.pss_kib;
                role.mem.uss_kib += m.uss_kib;
                snap.mem.rss_kib += m.rss_kib;
                snap.mem.pss_kib += m.pss_kib;
                snap.mem.uss_kib += m.uss_kib;
            }
            None => snap.unreadable += 1,
        }
    }
    snap
}

impl Snapshot {
    pub fn to_json(&self) -> Value {
        let roles: serde_json::Map<String, Value> = self
            .roles
            .iter()
            .map(|(name, total)| {
                (
                    name.clone(),
                    json!({
                        "processes": total.processes,
                        "rss_mib": mib(total.mem.rss_kib),
                        "pss_mib": mib(total.mem.pss_kib),
                        "uss_mib": mib(total.mem.uss_kib),
                    }),
                )
            })
            .collect();
        json!({
            "processes": self.processes,
            "rss_mib": mib(self.mem.rss_kib),
            "pss_mib": mib(self.mem.pss_kib),
            "uss_mib": mib(self.mem.uss_kib),
            "unreadable": self.unreadable,
            "roles": roles,
        })
    }
}

pub fn mib(kib: u64) -> f64 {
    ((kib as f64 / 1024.0) * 100.0).round() / 100.0
}

/// CPU used between two snapshots, as a percentage of one logical CPU. A
/// process that is new in `end` counts in full; one that exited is lost.
pub fn cpu_percent(start: &HashMap<i32, u64>, end: &HashMap<i32, u64>, wall_secs: f64) -> f64 {
    let ticks: u64 = end
        .iter()
        .map(|(pid, ticks)| ticks.saturating_sub(start.get(pid).copied().unwrap_or(0)))
        .sum();
    (ticks as f64 / CLK_TCK) / wall_secs * 100.0
}

/// This process: memory and ticks.
pub fn own() -> (Mem, u64) {
    let pid = std::process::id() as i32;
    (
        mem(pid).unwrap_or_default(),
        read_proc(pid).map(|p| p.ticks).unwrap_or(0),
    )
}
