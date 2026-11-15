//! Internal preflight helpers. This does not register or advertise restart.
use super::{CurrentElf, HandoffDescription, HandoffTarget, ProbeLimits};
use std::{
    future::Future,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

/// Resolved local allowlist entry, not a wire/request DTO.
pub struct TargetSelection<'a> {
    pub id: &'a str,
    pub configured_path: Option<&'a Path>,
    pub timeout: Duration,
}

pub struct TargetRequirements<'a> {
    pub handoff: u32,
    pub snapshot: u32,
    pub storage_schema: u32,
    pub protocol: u32,
    pub platform: &'a str,
}
/// One request's immutable local settings. Reloading the file cannot retarget it.
pub struct RestartTarget {
    id: String,
    timeout: Duration,
    path: PathBuf,
    current: bool,
    target: HandoffTarget,
}
pub struct CaptureCheckedTarget(RestartTarget);
impl RestartTarget {
    /// The coordinator strictly reloads local config and resolves its allowlist
    /// before calling this. Keeping config outside launcher preserves the client
    /// boundary; a wire adapter must never construct this from an arbitrary path.
    pub async fn preflight(
        selection: TargetSelection<'_>,
        current: &CurrentElf,
        requirements: TargetRequirements<'_>,
        limits: ProbeLimits,
        cancel: impl Future<Output = ()>,
    ) -> io::Result<Self> {
        let TargetSelection {
            id,
            configured_path,
            timeout,
        } = selection;
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            || timeout.is_zero()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid local target selection",
            ));
        }
        let path = match (id, configured_path) {
            ("current", None) => {
                current.revalidate()?;
                current.path().to_owned()
            }
            ("current", Some(_)) | (_, None) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid local target selection",
                ));
            }
            (_, Some(path)) => path.to_owned(),
        };
        let target = HandoffTarget::probe_cancellable(&path, limits, cancel).await?;
        target.description().require(
            requirements.handoff,
            requirements.snapshot,
            requirements.storage_schema,
            requirements.protocol,
            requirements.platform,
        )?;
        if id == "current" {
            current.revalidate()?;
        }
        Ok(Self {
            id: id.to_owned(),
            timeout,
            path,
            current: id == "current",
            target,
        })
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
    pub fn description(&self) -> &HandoffDescription {
        self.target.description()
    }
    pub fn requested_path(&self) -> &Path {
        &self.path
    }
    /// Stage 2: the last reversible point, before the spawn fence closes.
    pub async fn before_capture(
        self,
        current: &CurrentElf,
        limits: ProbeLimits,
        cancel: impl Future<Output = ()>,
    ) -> io::Result<CaptureCheckedTarget> {
        if self.current {
            current.revalidate()?;
        }
        self.target.reprobe_cancellable(limits, cancel).await?;
        Ok(CaptureCheckedTarget(self))
    }
}
impl CaptureCheckedTarget {
    pub fn request(&self) -> &RestartTarget {
        &self.0
    }
    /// Stage 3: no child is spawned. Any error after capture means FailedClosed,
    /// never permission to return to Running or automatically pick another build.
    pub fn before_exec(&self, current: &CurrentElf) -> io::Result<&HandoffTarget> {
        if self.0.current {
            current.revalidate()?;
        }
        self.0.target.revalidate()?;
        Ok(&self.0.target)
    }
}

/// Call at entry before creating a runtime, DB, owner, socket or ordinary service.
/// The description supplier is never evaluated for a normal boot.
pub fn describe_at_entry(
    entry: &core_instance::handoff::entry::EntryOptions,
    describe: impl FnOnce() -> io::Result<HandoffDescription>,
    mut output: impl io::Write,
) -> io::Result<bool> {
    if !entry.describe {
        return Ok(false);
    }
    let description = describe()?;
    description.validate()?;
    let data = serde_json::to_vec(&description)?;
    if data.len() > 16384 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "self-description too large",
        ));
    }
    output.write_all(&data)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_entry_does_not_evaluate_description_or_write_output() {
        let entry = core_instance::handoff::entry::EntryOptions::parse([]).unwrap();
        let mut output = Vec::new();
        assert!(
            !describe_at_entry(
                &entry,
                || panic!("ordinary bootstrap must not probe"),
                &mut output
            )
            .unwrap()
        );
        assert!(output.is_empty());
    }
}
