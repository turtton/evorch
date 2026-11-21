//! Linux process-tree assertions. Only the test process becomes a subreaper.
#![allow(dead_code)]
use rustix::{
    io::Errno,
    process::{Pid, WaitId, WaitIdOptions, waitid},
};
use std::collections::BTreeSet;
fn processes() -> Vec<(i32, i32, i32)> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            Some((
                pid,
                fields.get(1)?.parse().ok()?,
                fields.get(2)?.parse().ok()?,
            ))
        })
        .collect()
}
pub struct Tree {
    groups: BTreeSet<i32>,
    baseline: BTreeSet<i32>,
}
impl Tree {
    pub fn new() -> Self {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid())).unwrap();
        let me = std::process::id() as i32;
        let baseline = processes()
            .into_iter()
            .filter(|(_, ppid, _)| *ppid == me)
            .map(|(p, _, _)| p)
            .collect();
        Self {
            groups: BTreeSet::new(),
            baseline,
        }
    }
    pub fn track_pid(&mut self, pid: i32) {
        self.groups.insert(pid);
    }
    pub fn track(&mut self, ledger: &sandbox::SpawnLedger) {
        for child in ledger.children() {
            if let Some(pid) = child.leader {
                self.groups.insert(pid);
            }
        }
        let all = processes();
        for &group in &self.groups {
            let mut descendants = BTreeSet::from([group]);
            loop {
                let old = descendants.len();
                for &(pid, ppid, _) in &all {
                    if descendants.contains(&ppid) {
                        descendants.insert(pid);
                    }
                }
                if descendants.len() == old {
                    break;
                }
            }
            for &(pid, _, pgid) in &all {
                if descendants.contains(&pid) {
                    assert_eq!(
                        pgid, group,
                        "renderer descendant {pid} escaped its owned group"
                    );
                }
            }
        }
    }
    pub fn assert_clean(self) {
        for &group in &self.groups {
            loop {
                match waitid(WaitId::Pgid(Pid::from_raw(group)), WaitIdOptions::EXITED) {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(Errno::CHILD) => break,
                    other => panic!("reap renderer descendants: {other:?}"),
                }
            }
        }
        let me = std::process::id() as i32;
        for (pid, ppid, pgid) in processes() {
            assert!(
                !self.groups.contains(&pgid),
                "renderer descendant {pid} remains (including zombies)"
            );
            assert!(
                ppid != me || self.baseline.contains(&pid),
                "unreaped/escaped renderer descendant {pid} outside owned group {pgid}"
            );
        }
    }
}

/// Kill a real core-owner process, then wait for helper EOF shutdown, not time.
pub async fn core_eof(executable: std::path::PathBuf, profiles: &std::path::Path) {
    use tokio::io::AsyncBufReadExt;
    let mut tree = Tree::new();
    let harness = std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("evorch-renderer-test-helper")
        });
    // stdin is /dev/null, stdout is the readiness pipe, stderr is the test's own.
    let mut command = sandbox::owned::OwnedCommand::new(harness);
    command
        .arg("--owner-core")
        .arg(executable)
        .arg(profiles)
        .env("EVORCH_RENDERER_TEST_SECRET", "must-not-be-inherited")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    let mut core = sandbox::owned::OwnedChild::spawn(&mut command).unwrap();
    let mut output = tokio::io::BufReader::new(core.take_stdout().unwrap());
    let mut line = String::new();
    assert!(
        output.read_line(&mut line).await.unwrap() > 0,
        "core owner did not report readiness"
    );
    tree.track_pid(line.trim().parse().unwrap());
    // The core owner's own guard helper (a child of the owner, in its group) is
    // orphaned to this subreaper when the owner dies and is reaped through the
    // owner's group below.
    tree.track_pid(core.id().unwrap() as i32);
    core.kill_and_wait().await.unwrap();
    // Core is dead, so the orphan helper belongs to this subreaper. Blocking
    // waitid is safe on this test-only thread; the helper needs no core work.
    tree.assert_clean();
    // Real CEF removes its profile on orderly EOF; the fake intentionally
    // leaves a confirmed generation to exercise next-core collection.
    let old: Vec<_> = std::fs::read_dir(profiles)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let tasks = workbench_host::areas::AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = workbench_host::areas::browser::renderer::RendererOwner::new(
        &tasks,
        ledger.clone(),
        workbench_host::areas::browser::renderer::OwnerConfig::new(
            std::env::current_exe()
                .unwrap()
                .with_file_name("nonexistent-renderer"),
            profiles.to_path_buf(),
        ),
    )
    .unwrap();
    let (o, b) = renderer_ipc::OpenArtifact::with_files(
        renderer_ipc::SessionId(1),
        renderer_ipc::TabId(1),
        renderer_ipc::Viewport {
            width: 1,
            height: 1,
        },
        30,
        30_000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            b"<p>next core</p>".to_vec(),
        )],
    );
    assert!(owner.open(o, b).await.is_err()); // collection runs before the missing binary fails
    for path in old {
        assert!(
            !path.exists(),
            "next core must collect old generation profile"
        );
    }
    // Only the new unconfirmed generation may remain; never reuse the old one.
    for entry in std::fs::read_dir(profiles).unwrap().flatten() {
        assert!(!entry.path().join("confirmed").exists());
    }
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    ledger.join_workers().await.unwrap();
}
