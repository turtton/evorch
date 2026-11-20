//! The product process: the artifact started through its ordinary entry with
//! `--config-dir`, in an isolated environment, observed from outside.
use super::{
    artifact::ProductArtifact,
    env::{Footprint, IsolatedEnv},
    names::ENV_RENDERER_HELPER,
    observe::{self, FdInfo, ProcInfo},
    process::{LaunchSpec, ManagedProcess, Report, WaitError},
};
use std::{
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};

/// Environment variable the product reads its provider key from. The value is
/// meaningless; the mock provider does not check it.
pub const KEY_ENV: &str = "EVORCH_F4P_PROVIDER_KEY";

/// The deadline of the failure watchdog for a product to become ready.
pub const READY_WATCHDOG: Duration = Duration::from_secs(120);
pub const EXIT_WATCHDOG: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Ready {
    /// Instant the reader thread saw the readiness line.
    pub at: Instant,
    pub socket: PathBuf,
    /// The config identity the core reports.
    pub core: String,
    pub web: Option<String>,
}

#[derive(Debug)]
pub struct Observation {
    pub pid: i32,
    /// `/proc/<pid>/exe`.
    pub exe: Option<PathBuf>,
    /// Same device and inode as the verified artifact.
    pub exe_is_artifact: bool,
    pub artifact_identity: String,
    pub children: Vec<ProcInfo>,
    pub fds: Vec<FdInfo>,
}

pub struct ProductLaunch<'a> {
    pub artifact: &'a ProductArtifact,
    pub env: &'a IsolatedEnv,
    pub args: Vec<OsString>,
    pub renderer_helper: Option<PathBuf>,
    pub extra_env: Vec<(String, OsString)>,
}
impl<'a> ProductLaunch<'a> {
    pub fn new(artifact: &'a ProductArtifact, env: &'a IsolatedEnv) -> Self {
        Self {
            artifact,
            env,
            args: Vec::new(),
            renderer_helper: None,
            extra_env: Vec::new(),
        }
    }
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn renderer_helper(mut self, helper: PathBuf) -> Self {
        self.renderer_helper = Some(helper);
        self
    }
    pub fn env(mut self, name: &str, value: impl Into<OsString>) -> Self {
        self.extra_env.push((name.into(), value.into()));
        self
    }

    /// Starts the product. The artifact is checked for modification first; the
    /// full hash was verified when it was opened.
    pub fn start(self) -> std::io::Result<ProductProcess<'a>> {
        self.artifact.assert_unchanged();
        let mut command = LaunchSpec::new(&self.artifact.path);
        command
            .arg("--config-dir")
            .arg(self.env.config_dir())
            .args(&self.args);
        let mut extra = vec![(KEY_ENV, OsString::from("f4p-offline-key"))];
        if let Some(helper) = &self.renderer_helper {
            extra.push(("EVORCH_RENDERER_EXEC", helper.clone().into_os_string()));
        }
        for (name, value) in &self.extra_env {
            extra.push((name.as_str(), value.clone()));
        }
        self.env.apply(&mut command, &extra);
        let process = ManagedProcess::spawn(command)?;
        // The instant the reaper reported the leader started: the reaper's own
        // start-up is not part of the product's.
        let started = process.spawned_at();
        Ok(ProductProcess {
            artifact: self.artifact,
            env: self.env,
            process,
            started,
            ready: None,
        })
    }
}

/// Name of the variable that selects the helper for the fixture's own tests.
pub fn helper_from_env() -> Option<PathBuf> {
    std::env::var_os(ENV_RENDERER_HELPER)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub struct ProductProcess<'a> {
    artifact: &'a ProductArtifact,
    env: &'a IsolatedEnv,
    pub process: ManagedProcess,
    /// Instant the leader was reported started (see `ManagedProcess::spawned_at`).
    pub started: Instant,
    ready: Option<Ready>,
}

#[derive(Debug)]
pub enum LaunchError {
    Wait(WaitError),
    BadReadyLine(String),
}
impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl ProductProcess<'_> {
    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    /// Waits for the readiness line, which the core prints only after both of
    /// its listeners are bound. A product that exits first is an `Exited`
    /// error with its stderr, not a timeout.
    pub fn wait_ready(&mut self) -> Result<Ready, LaunchError> {
        let (value, at) = self
            .process
            .wait_for(READY_WATCHDOG, |line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .filter(|v| v["status"] == "ready")
            })
            .map_err(LaunchError::Wait)?;
        let ready = Ready {
            at,
            socket: value["socket"]
                .as_str()
                .ok_or_else(|| LaunchError::BadReadyLine(value.to_string()))?
                .into(),
            core: value["core"]
                .as_str()
                .ok_or_else(|| LaunchError::BadReadyLine(value.to_string()))?
                .to_owned(),
            web: value["web"].as_str().map(str::to_owned),
        };
        self.ready = Some(ready.clone());
        Ok(ready)
    }

    pub fn ready(&self) -> &Ready {
        self.ready.as_ref().expect("wait_ready succeeded")
    }

    /// PID, image, children and descriptors right now.
    pub fn observe(&mut self) -> Observation {
        let pid = self.pid();
        let exe = observe::process(pid).and_then(|p| p.exe);
        let exe_is_artifact = std::fs::metadata(format!("/proc/{pid}/exe"))
            .ok()
            .is_some_and(|_| {
                observe::same_file(
                    std::path::Path::new(&format!("/proc/{pid}/exe")),
                    &self.artifact.path,
                )
            });
        Observation {
            pid,
            exe,
            exe_is_artifact,
            artifact_identity: self.artifact.build_identity().to_owned(),
            children: observe::descendants(pid),
            fds: observe::open_fds(pid).unwrap_or_default(),
        }
    }

    /// SIGTERM, wait for the exit notification, join, and report. The report
    /// carries any descendant that outlived the product.
    pub fn stop(mut self) -> (Report, Footprint) {
        let footprint = self.env.footprint();
        let _ = self.process.terminate(EXIT_WATCHDOG);
        (self.process.join(), footprint)
    }

    /// The product exited by itself (or failed to start).
    pub fn finish(self) -> (Report, Footprint) {
        let footprint = self.env.footprint();
        (self.process.join(), footprint)
    }
}
