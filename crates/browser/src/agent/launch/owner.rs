use super::super::{AgentError, Proxy};
use super::{args, endpoint};
use sandbox::{
    ledger::{Admission, ChildKind},
    owned::{OwnedChild, OwnedCommand},
};
use std::{io, path::PathBuf, process::Stdio};
use tokio::{sync::oneshot, time::Instant};

pub(in super::super) struct Chromium {
    child: Option<OwnedChild>,
    resources: Option<Resources>,
}
struct Resources {
    profile: Option<tempfile::TempDir>,
    _proxy: Proxy,
}
impl Resources {
    fn unconfirmed(&mut self) {
        // GuardLost is NOT confirmed reap. Do not delete a live profile.
        if let Some(profile) = self.profile.take() {
            let path = profile.keep();
            tracing::error!(?path, "Chromium reap unconfirmed; retaining profile");
        }
    }
}
impl Chromium {
    pub(in super::super) async fn launch(
        executable: PathBuf,
        profile: tempfile::TempDir,
        proxy: Proxy,
    ) -> Result<(Self, String), AgentError> {
        let (mut owner, deadline) = Self::spawn(executable, profile, proxy).await?;
        let stderr = owner
            .child
            .as_mut()
            .expect("spawned")
            .take_stderr()
            .expect("piped stderr");
        // The reader is dropped on success and on every failure; no drain task.
        let endpoint = endpoint::read(stderr, deadline)
            .await
            .map_err(launch_error)?;
        Ok((owner, endpoint))
    }

    async fn spawn(
        executable: PathBuf,
        profile: tempfile::TempDir,
        proxy: Proxy,
    ) -> Result<(Self, Instant), AgentError> {
        // All resources travel WITH the blocking launch result, including when
        // its awaiting task is cancelled before the authenticated ExecOk.
        tokio::task::spawn_blocking(move || {
            let mut resources = Resources {
                profile: Some(profile),
                _proxy: proxy,
            };
            let path = executable_path(executable)?;
            let mut command = OwnedCommand::new(path);
            command
                .args(args::arguments(
                    resources.profile.as_ref().expect("profile").path(),
                    resources._proxy.address(),
                ))
                .stdin(Stdio::inherit())
                .stdout(Stdio::null())
                .stderr(Stdio::piped());
            let child = OwnedChild::spawn_in(
                &Admission::unmanaged(),
                ChildKind::Other,
                Some("agent-chromium"),
                &mut command,
            )
            .inspect_err(|_| {
                // The typed spawn API can fail while converting post-ExecOk
                // pipes/monitors. It exposes no reap confirmation on Err.
                resources.unconfirmed();
            })?;
            let deadline = Instant::now() + endpoint::TIMEOUT; // immediately after ExecOk
            Ok((
                Self {
                    child: Some(child),
                    resources: Some(resources),
                },
                deadline,
            ))
        })
        .await
        .map_err(|error| AgentError::Launch(error.to_string()))?
        .map_err(launch_error)
    }

    /// Drop initiates cleanup; its confirmation can also be awaited by tests.
    /// Never block an async worker on the synchronous guard decoder/reaper.
    pub(super) fn retire(&mut self) -> Option<oneshot::Receiver<io::Result<()>>> {
        let child = self.child.take()?;
        let resources = self.resources.take().expect("child owns resources");
        let (reply, receive) = oneshot::channel();
        let job = ReapJob {
            child,
            resources,
            confirmed: false,
        };
        // A standalone reaper still works after the Tokio runtime has stopped.
        // If thread creation fails, ReapJob::drop retains the unconfirmed profile
        // and OwnedChild's fallback retains ownership of group cleanup.
        if let Err(error) = std::thread::Builder::new()
            .name("agent-chromium-reap".into())
            .spawn(move || {
                let mut job = job;
                let result = job.child.kill_and_reap_blocking();
                job.confirmed = result.is_ok();
                drop(job); // profile/proxy only AFTER confirmation (or retention)
                let _ = reply.send(result);
            })
        {
            tracing::error!(%error, "could not start Chromium reaper");
        }
        Some(receive)
    }
}
struct ReapJob {
    child: OwnedChild,
    resources: Resources,
    confirmed: bool,
}
impl Drop for ReapJob {
    fn drop(&mut self) {
        if !self.confirmed {
            self.resources.unconfirmed();
        }
    }
}
impl Drop for Chromium {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}
#[cfg(all(test, target_os = "linux"))]
#[path = "owner_tests.rs"]
mod tests;

fn launch_error(error: io::Error) -> AgentError {
    AgentError::Launch(error.to_string())
}

fn executable_path(path: PathBuf) -> io::Result<PathBuf> {
    // Match chromiumoxide 0.9.1's canonicalize_except_snap, including the
    // original absolute launcher path for snap (never exec /usr/bin/snap itself).
    let canonical = std::fs::canonicalize(&path)?;
    let canonical = dunce::simplified(&canonical).to_path_buf();
    if canonical.to_string_lossy().ends_with("/snap") {
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(dunce::simplified(&absolute).to_path_buf())
    } else {
        Ok(canonical)
    }
}
