//! Target probing and revalidation. No runtime/storage dependency, target
//! configuration, production request entry, or capability advertisement.
use crate::CoreBinary;
use core_instance::{
    Instance,
    handoff::{
        BlockedShutdownSignals, HandoffRecord, elf_identity, preflight_exec, running_elf_identity,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    ffi::OsString,
    fs::File,
    future::Future,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};
mod supervisor;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaRange {
    pub minimum: u32,
    pub maximum: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffDescription {
    pub version: u32,
    /// Final ELF, not the entrypoint wrapper.
    pub elf_path: PathBuf,
    pub build_identity: String,
    pub handoff_format: u32,
    pub snapshot_format: u32,
    pub schema: SchemaRange,
    pub protocol: u32,
    pub platform: String,
    pub tool_catalog: String,
}
impl HandoffDescription {
    /// Used by a self-description entry *before* all ordinary initialization.
    pub fn for_running(
        handoff_format: u32,
        snapshot_format: u32,
        schema: SchemaRange,
        protocol: u32,
        tool_catalog: String,
    ) -> io::Result<Self> {
        Ok(Self {
            version: 1,
            elf_path: std::env::current_exe()?,
            build_identity: running_elf_identity()?,
            handoff_format,
            snapshot_format,
            schema,
            protocol,
            platform: format!("linux-{}", std::env::consts::ARCH),
            tool_catalog,
        })
    }
    pub fn require(
        &self,
        handoff: u32,
        snapshot: u32,
        schema: u32,
        protocol: u32,
        platform: &str,
    ) -> io::Result<()> {
        if self.handoff_format != handoff
            || self.snapshot_format != snapshot
            || schema < self.schema.minimum
            || schema > self.schema.maximum
            || self.protocol != protocol
            || self.platform != platform
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "incompatible handoff target",
            ));
        }
        Ok(())
    }
    fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.handoff_format == 0
            || self.snapshot_format == 0
            || self.protocol == 0
            || self.schema.minimum > self.schema.maximum
            || self.schema.minimum == 0
            || [&self.build_identity, &self.platform, &self.tool_catalog]
                .iter()
                .any(|s| s.is_empty() || s.len() > 256 || s.contains('\0'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid self-description",
            ));
        }
        let elf = CoreBinary::explicit(&self.elf_path)?;
        if elf_identity(&elf.0)? != self.build_identity {
            return Err(changed());
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ProbeLimits {
    pub timeout: Duration,
    pub output_bytes: usize,
}
impl Default for ProbeLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            output_bytes: 16384,
        }
    }
}
impl ProbeLimits {
    fn validate(self) -> io::Result<Self> {
        if self.timeout.is_zero()
            || self.timeout > Duration::from_secs(60)
            || self.output_bytes == 0
            || self.output_bytes > 65536
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "probe limits: 1..=65536 bytes and (0,60s]",
            ));
        }
        Ok(self)
    }
}
#[derive(Debug, PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    length: u64,
    ctime: i64,
    ctime_nsec: i64,
    digest: String,
}
impl Fingerprint {
    fn read(binary: &CoreBinary) -> io::Result<Self> {
        let mut file = File::open(&binary.0)?;
        let m = file.metadata()?;
        let mut hash = Sha256::new();
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            length: m.len(),
            ctime: m.ctime(),
            ctime_nsec: m.ctime_nsec(),
            digest: format!("{:x}", hash.finalize()),
        })
    }
}
fn changed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "handoff target changed")
}

/// ELF recorded at startup, not the wrapper used to start this process. A
/// replaced/deleted path never silently resolves to a different current build.
pub struct CurrentElf {
    path: PathBuf,
    fingerprint: Fingerprint,
}
impl CurrentElf {
    pub fn record() -> io::Result<Self> {
        let current = Self::record_path(&std::env::current_exe()?)?;
        let live = std::fs::metadata("/proc/self/exe")?;
        if current.fingerprint.device != live.dev()
            || current.fingerprint.inode != live.ino()
            || format!("sha256:{}", current.fingerprint.digest) != running_elf_identity()?
        {
            return Err(changed());
        }
        Ok(current)
    }
    fn record_path(path: &Path) -> io::Result<Self> {
        let binary = CoreBinary::explicit(path)?;
        elf_identity(&binary.0)?;
        Ok(Self {
            fingerprint: Fingerprint::read(&binary)?,
            path: binary.0,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn revalidate(&self) -> io::Result<CoreBinary> {
        let binary = CoreBinary::explicit(&self.path)?;
        if Fingerprint::read(&binary)? != self.fingerprint {
            return Err(changed());
        }
        Ok(binary)
    }
}

/// A probe observation, not an exec authorization. 4b must compare compatibility
/// and re-probe at the final reversible point, then perform durable readback.
pub struct HandoffTarget {
    requested: PathBuf,
    binary: CoreBinary,
    entry: Fingerprint,
    elf: Fingerprint,
    description: HandoffDescription,
}
impl HandoffTarget {
    pub async fn probe(path: &Path, limits: ProbeLimits) -> io::Result<Self> {
        Self::probe_cancellable(path, limits, std::future::pending()).await
    }
    pub async fn probe_cancellable(
        path: &Path,
        limits: ProbeLimits,
        cancel: impl Future<Output = ()>,
    ) -> io::Result<Self> {
        let limits = limits.validate()?;
        preflight_exec()?;
        let binary = CoreBinary::explicit(path)?;
        let entry = Fingerprint::read(&binary)?;
        let output = supervisor::probe(&binary.0, limits, cancel).await?;
        let description: HandoffDescription = serde_json::from_slice(&output)?;
        description.validate()?;
        let elf = Fingerprint::read(&CoreBinary::explicit(&description.elf_path)?)?;
        let target = Self {
            requested: path.to_owned(),
            binary,
            entry,
            elf,
            description,
        };
        target.revalidate()?;
        Ok(target)
    }
    pub fn description(&self) -> &HandoffDescription {
        &self.description
    }
    pub fn canonical_entrypoint(&self) -> &Path {
        &self.binary.0
    }
    /// Capture-time re-probe. A changed build/catalog/format is a reversible
    /// cancellation, not permission to replace the recorded transaction target.
    pub async fn reprobe(&self, limits: ProbeLimits) -> io::Result<()> {
        let next = Self::probe(&self.requested, limits).await?;
        if next.description != self.description || next.entry != self.entry || next.elf != self.elf
        {
            return Err(changed());
        }
        Ok(())
    }
    pub fn revalidate(&self) -> io::Result<()> {
        let binary = CoreBinary::explicit(&self.requested)?;
        if binary.0 != self.binary.0
            || Fingerprint::read(&binary)? != self.entry
            || Fingerprint::read(&CoreBinary::explicit(&self.description.elf_path)?)? != self.elf
        {
            return Err(changed());
        }
        Ok(())
    }
    /// # Safety
    /// All Instance::exec_handoff final-fence requirements apply. An error is
    /// FailedClosed; retain Instance, never acquire again or retry automatically.
    pub unsafe fn exec(
        &self,
        instance: &Instance,
        signals: &BlockedShutdownSignals,
        args: &[OsString],
        record: HandoffRecord,
    ) -> io::Result<Infallible> {
        self.revalidate()?;
        if record.successor_build != self.description.build_identity {
            return Err(changed());
        }
        unsafe { instance.exec_handoff(signals, &self.binary.0, args, record) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn current_rejects_identical_bytes_in_a_replacement_inode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("elf");
        fs::copy(std::env::current_exe().unwrap(), &p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        let current = CurrentElf::record_path(&p).unwrap();
        let replacement = dir.path().join("replacement");
        fs::copy(&p, &replacement).unwrap();
        fs::rename(replacement, &p).unwrap();
        assert!(current.revalidate().is_err());
        fs::remove_file(&p).unwrap();
        assert!(current.revalidate().is_err());
    }
    #[test]
    fn compatibility_checks_inclusive_schema_edges() {
        let d = HandoffDescription::for_running(
            1,
            2,
            SchemaRange {
                minimum: 3,
                maximum: 5,
            },
            6,
            "catalog".into(),
        )
        .unwrap();
        for schema in [3, 4, 5] {
            d.require(1, 2, schema, 6, &d.platform).unwrap();
        }
        for schema in [2, 6] {
            assert!(d.require(1, 2, schema, 6, &d.platform).is_err());
        }
        assert!(d.require(9, 2, 4, 6, &d.platform).is_err());
        assert!(d.require(1, 9, 4, 6, &d.platform).is_err());
        assert!(d.require(1, 2, 4, 9, &d.platform).is_err());
        assert!(d.require(1, 2, 4, 6, "other").is_err());
    }
}
