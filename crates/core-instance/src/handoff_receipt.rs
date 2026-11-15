//! Diagnostic only. Neither this file nor its fields grant recovery authority.
use super::boot::BootFailure;
use crate::{ConfigIdentity, regular, unsafe_path};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::PathBuf,
};
pub const RECEIPT_LIMIT: usize = 16 * 1024;
const NAME: &str = ".evorch-core.restart-receipt";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStart {
    pub boot_id: String,
    pub start_ticks: u64,
}
impl ProcessStart {
    pub fn current() -> io::Result<Self> {
        let stat = fs::read_to_string("/proc/self/stat")?;
        let fields = stat.rsplit_once(')').ok_or_else(unsafe_path)?.1;
        let start_ticks = fields
            .split_whitespace()
            .nth(19)
            .and_then(|s| s.parse().ok())
            .ok_or_else(unsafe_path)?;
        Ok(Self {
            boot_id: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .to_owned(),
            start_ticks,
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPhase {
    Armed,
    Adopted,
    Completed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootStatus {
    /// A surviving pre-exec receipt does not prove that exec or Ready happened.
    Restarting,
    Failed {
        failure: BootFailure,
        exit_code: i32,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    WaitForReady,
    ExplicitColdStart,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBootReceipt {
    pub version: u32,
    pub config_identity: String,
    pub transaction_id: String,
    pub predecessor_epoch: u64,
    pub successor_epoch: u64,
    pub pid: u32,
    pub process_start: ProcessStart,
    /// None explicitly means the durable phase could not be confirmed.
    pub confirmed_phase: Option<BootPhase>,
    pub status: BootStatus,
    pub revision: u64,
    pub next_action: NextAction,
}
impl RestartBootReceipt {
    pub fn restarting(
        config: &ConfigIdentity,
        transaction_id: String,
        predecessor_epoch: u64,
        successor_epoch: u64,
    ) -> io::Result<Self> {
        let receipt = Self {
            version: 1,
            config_identity: config.identity().to_owned(),
            transaction_id,
            predecessor_epoch,
            successor_epoch,
            pid: std::process::id(),
            process_start: ProcessStart::current()?,
            confirmed_phase: Some(BootPhase::Armed),
            status: BootStatus::Restarting,
            revision: 1,
            next_action: NextAction::WaitForReady,
        };
        receipt.validate()?;
        Ok(receipt)
    }
    pub fn failed(&mut self, failure: BootFailure, phase: Option<BootPhase>) -> io::Result<()> {
        self.revision = self.revision.checked_add(1).ok_or_else(unsafe_path)?;
        self.confirmed_phase = phase;
        self.status = BootStatus::Failed {
            failure,
            exit_code: failure.exit_code(),
        };
        self.next_action = NextAction::ExplicitColdStart;
        self.validate()
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || self.revision == 0
            || self.pid == 0
            || self.successor_epoch <= self.predecessor_epoch
            || self.process_start.start_ticks == 0
            || [
                &self.config_identity,
                &self.transaction_id,
                &self.process_start.boot_id,
            ]
            .iter()
            .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(unsafe_path());
        }
        match self.status {
            BootStatus::Restarting if self.next_action == NextAction::WaitForReady => Ok(()),
            BootStatus::Failed { failure, exit_code }
                if exit_code == failure.exit_code()
                    && self.next_action == NextAction::ExplicitColdStart =>
            {
                Ok(())
            }
            _ => Err(unsafe_path()),
        }
    }
}

impl ConfigIdentity {
    /// Durable config directory, never the ephemeral socket/runtime directory.
    pub fn restart_receipt_path(&self) -> PathBuf {
        self.config_dir().join(NAME)
    }

    /// Caller must retain process ownership and serialize revisions. The writer
    /// is synchronous; invoke only outside the deadline monitor. IO failure must
    /// not turn into a retry with a fresh deadline.
    pub fn write_restart_receipt(&self, receipt: &RestartBootReceipt) -> io::Result<()> {
        receipt.validate()?;
        if receipt.config_identity != self.identity() {
            return Err(unsafe_path());
        }
        let data = serde_json::to_vec(receipt)?;
        if data.len() > RECEIPT_LIMIT {
            return Err(unsafe_path());
        }
        let (dir, anchored) = self.receipt_directory()?;
        let dest = anchored.join(NAME);
        match open_receipt(&dest) {
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&anchored)?;
        // NamedTempFile creates 0600 atomically; do not loosen permissions.
        temporary.write_all(&data)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&dest).map_err(|e| e.error)?;
        dir.sync_all()
    }
    pub fn read_restart_receipt(&self) -> io::Result<Option<RestartBootReceipt>> {
        let (_dir, anchored) = self.receipt_directory()?;
        let mut file = match open_receipt(&anchored.join(NAME)) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut data = Vec::new();
        // Metadata can race a local writer; cap the actual read too.
        (&mut file)
            .take((RECEIPT_LIMIT + 1) as u64)
            .read_to_end(&mut data)?;
        if data.len() > RECEIPT_LIMIT {
            return Err(unsafe_path());
        }
        let receipt: RestartBootReceipt = serde_json::from_slice(&data)?;
        receipt.validate()?;
        if receipt.config_identity != self.identity() {
            return Err(unsafe_path());
        }
        Ok(Some(receipt))
    }
    fn receipt_directory(&self) -> io::Result<(File, PathBuf)> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.config_dir())?;
        let meta = dir.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
            return Err(unsafe_path());
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        Ok((dir, anchored))
    }
}
fn open_receipt(path: &std::path::Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    regular(&file, path)?;
    if file.metadata()?.len() > RECEIPT_LIMIT as u64 {
        return Err(unsafe_path());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    #[test]
    fn durable_diagnostic_is_private_and_atomically_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        let mut receipt = RestartBootReceipt::restarting(&config, "tx".into(), 1, 2).unwrap();
        config.write_restart_receipt(&receipt).unwrap();
        let path = config.restart_receipt_path();
        assert_eq!(path.parent(), Some(config.config_dir()));
        let old = File::open(&path).unwrap();
        receipt
            .failed(BootFailure::AdoptedBeforeReady, Some(BootPhase::Adopted))
            .unwrap();
        config.write_restart_receipt(&receipt).unwrap();
        assert_ne!(
            old.metadata().unwrap().ino(),
            fs::metadata(&path).unwrap().ino()
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(config.read_restart_receipt().unwrap(), Some(receipt));
        let old_receipt: RestartBootReceipt = serde_json::from_reader(old).unwrap();
        assert_eq!(old_receipt.status, BootStatus::Restarting);
    }
    #[test]
    fn corrupt_or_unsafe_diagnostics_are_never_partially_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        let path = config.restart_receipt_path();
        let receipt = RestartBootReceipt::restarting(&config, "tx".into(), 1, 2).unwrap();
        assert!(config.read_restart_receipt().unwrap().is_none());
        for data in [b"{".to_vec(), vec![b'x'; RECEIPT_LIMIT + 1]] {
            fs::write(&path, data).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(config.read_restart_receipt().is_err());
            fs::remove_file(&path).unwrap();
        }
        for field in ["version", "config_identity"] {
            let mut value = serde_json::to_value(&receipt).unwrap();
            value[field] = if field == "version" {
                2.into()
            } else {
                "wrong".into()
            };
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(config.read_restart_receipt().is_err());
            fs::remove_file(&path).unwrap();
        }
        let outside = dir.path().join("sentinel");
        fs::write(&outside, "unchanged").unwrap();
        symlink(&outside, &path).unwrap();
        assert!(config.read_restart_receipt().is_err());
        assert!(config.write_restart_receipt(&receipt).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");
        fs::remove_file(&path).unwrap();
        config.write_restart_receipt(&receipt).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(config.read_restart_receipt().is_err());
        assert!(config.write_restart_receipt(&receipt).is_err());
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(config.read_restart_receipt().is_err());
        assert!(config.write_restart_receipt(&receipt).is_err());
    }
    #[test]
    fn failed_write_preserves_preexec_diagnostic_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        let receipt = RestartBootReceipt::restarting(&config, "tx".into(), 1, 2).unwrap();
        config.write_restart_receipt(&receipt).unwrap();
        let mut invalid = receipt.clone();
        invalid.status = BootStatus::Failed {
            failure: BootFailure::BeforeAdopt,
            exit_code: 0,
        };
        assert!(config.write_restart_receipt(&invalid).is_err());
        assert_eq!(config.read_restart_receipt().unwrap(), Some(receipt));
        fs::remove_file(config.restart_receipt_path()).unwrap();
        fs::create_dir(config.restart_receipt_path()).unwrap();
        assert!(config.write_restart_receipt(&invalid).is_err());
        assert!(
            config
                .write_restart_receipt(
                    &RestartBootReceipt::restarting(&config, "tx".into(), 1, 2).unwrap()
                )
                .is_err()
        );
    }
}
