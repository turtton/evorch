//! Spawning and stopping the real `evorch-renderer`.
//!
//! Mirrors the launch policy of the renderer's own test harness: the control
//! channel is the helper's stdin and the frame channel its stdout, the helper
//! leads its own process group, and this process is a child subreaper so that
//! CEF descendants orphaned by a dying parent can be reaped and counted.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use renderer_ipc::{ClientConfig, CloseReason, RendererClient, RendererGeneration, stdio_channels};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};

use crate::procfs;

pub struct Config {
    /// The `evorch-renderer` binary; `evorch-cef-helper` must sit next to it.
    pub renderer: PathBuf,
    /// Parent of every run directory. Chromium binds a Unix socket below
    /// `TMPDIR`, whose path limit a long directory exceeds, so the helper's
    /// `TMPDIR` is `/proc/self/cwd` and its working directory is the run
    /// directory.
    pub work_dir: PathBuf,
}

pub fn become_subreaper() {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid()))
        .expect("become a child subreaper");
}

/// A run directory that is removed on drop.
pub struct RunDir(pub PathBuf);

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Helper {
    pub child: Child,
    pub pid: i32,
    group: Pid,
}

impl Helper {
    pub fn snapshot(&self) -> procfs::Snapshot {
        procfs::snapshot(self.pid, self.pid)
    }

    /// Reaps every process of the group that is not the direct child and
    /// returns how many there were. Blocks until none is left.
    pub fn reap_descendants(&self) -> usize {
        let mut reaped = 0;
        loop {
            match waitid(WaitId::Pgid(Some(self.group)), WaitIdOptions::EXITED) {
                Ok(_) => reaped += 1,
                Err(Errno::INTR) => {}
                Err(Errno::CHILD) => return reaped,
                Err(error) => panic!("waiting for the helper's group: {error}"),
            }
        }
    }

    pub fn live_members(&self) -> Vec<i32> {
        let group = self.pid;
        procfs::scan_all()
            .into_iter()
            .filter(|p| p.pgrp == group && p.state != 'Z')
            .map(|p| p.pid)
            .collect()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = kill_process_group(self.group, Signal::KILL);
        let _ = self.child.wait();
        self.reap_descendants();
    }
}

/// Field order is drop order: the client closes its channels first, then the
/// group is stopped and reaped, and the run directory goes last.
pub struct Launched {
    pub client: RendererClient,
    pub helper: Helper,
    #[allow(dead_code)]
    pub dir: RunDir,
    pub spawn_ms: f64,
    /// From before `spawn` until the `Hello` answer: process start, CEF
    /// initialization and the handshake.
    pub hello_ms: f64,
}

fn run_dir(config: &Config, label: &str) -> Result<RunDir, String> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = config
        .work_dir
        .join(format!("mr-{}-{n}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(RunDir(dir))
}

pub fn launch(config: &Config, label: &str, generation: u64) -> Result<Launched, String> {
    let cef_helper = config.renderer.with_file_name("evorch-cef-helper");
    if !cef_helper.is_file() {
        return Err(format!(
            "{} is missing: build it next to the renderer",
            cef_helper.display()
        ));
    }
    let dir = run_dir(config, label)?;
    let stderr = File::create(dir.0.join("helper.stderr")).map_err(|e| e.to_string())?;
    let channels = stdio_channels().map_err(|e| e.to_string())?;
    let mut command = Command::new(&config.renderer);
    command
        .arg("--cache-dir")
        .arg(dir.0.join("profile"))
        .arg("--log-file")
        .arg(dir.0.join("cef.log"))
        .current_dir(&dir.0)
        .env("TMPDIR", "/proc/self/cwd")
        .env("EVORCH_CEF_HELPER", &cef_helper)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_SESSION_TYPE")
        .process_group(0)
        .stdin(channels.helper_stdin)
        .stdout(channels.helper_stdout)
        .stderr(Stdio::from(stderr));
    let started = Instant::now();
    let child = command
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", config.renderer.display()))?;
    let spawn_ms = ms(started.elapsed());
    // `command` still owns the helper's ends; dropping it closes this side's
    // copies, so the helper's exit reads as a closed channel.
    drop(command);
    let pid = child.id() as i32;
    let helper = Helper {
        group: Pid::from_raw(pid).expect("a child has a pid"),
        pid,
        child,
    };
    let (client, _info) = RendererClient::connect(
        channels.control,
        channels.frames,
        ClientConfig::new(RendererGeneration(generation)),
    )
    .map_err(|error| {
        let log = std::fs::read_to_string(dir.0.join("helper.stderr")).unwrap_or_default();
        format!("the helper did not come up: {error}; stderr: {log}")
    })?;
    Ok(Launched {
        hello_ms: ms(started.elapsed()),
        spawn_ms,
        client,
        helper,
        dir,
    })
}

pub fn ms(duration: Duration) -> f64 {
    crate::stats::round(duration.as_secs_f64() * 1000.0)
}

#[derive(Debug)]
pub struct StopTimings {
    /// Until `client.shutdown` returned (orderly stop only; 0 when forced).
    pub ack_ms: f64,
    /// From the start of the stop until the helper process had exited.
    pub exit_ms: f64,
    /// From the start of the stop until every group member was reaped.
    pub reaped_ms: f64,
    pub close: String,
    pub exit_status: String,
    /// Descendants that were reaped after the helper itself had exited.
    pub reaped_orphans: usize,
    /// Live group members and live children of this process afterwards.
    pub leftover: usize,
}

fn finish(l: &mut Launched, started: Instant, ack_ms: f64, close: String) -> StopTimings {
    let status: ExitStatus = l.helper.child.wait().expect("wait for the helper");
    let exit_ms = ms(started.elapsed());
    let reaped_orphans = l.helper.reap_descendants();
    let reaped_ms = ms(started.elapsed());
    let leftover = l.helper.live_members().len() + procfs::children_of_self().len();
    StopTimings {
        ack_ms,
        exit_ms,
        reaped_ms,
        close,
        exit_status: format!("{status}"),
        reaped_orphans,
        leftover,
    }
}

/// `Shutdown` request, then wait for the exit and for every descendant.
pub fn stop_orderly(l: &mut Launched) -> StopTimings {
    let started = Instant::now();
    let close = l.client.shutdown(Duration::from_secs(60));
    let ack_ms = ms(started.elapsed());
    let close = format!("{close:?}");
    debug_assert!(close.contains("Shutdown") || close.contains("Local"));
    let _ = CloseReason::LocalClose;
    finish(l, started, ack_ms, close)
}

/// SIGKILL to the whole group, as a crash of the core would not do but a
/// supervisor's final stop would.
pub fn stop_forced(l: &mut Launched) -> StopTimings {
    let started = Instant::now();
    let _ = kill_process_group(l.helper.group, Signal::KILL);
    finish(l, started, 0.0, "killed".into())
}
