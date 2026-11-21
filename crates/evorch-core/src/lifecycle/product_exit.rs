//! Trusted product owners retained from entry; no second lock or signal reader.
use super::{OperatorSignals, control::ControlDrainHandle};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Allowlisted startup options, never copied from raw argv or management DTOs.
pub struct ProductOptions {
    config_dir: PathBuf,
    web: bool,
}
impl ProductOptions {
    /// config_dir is the canonical directory obtained from ConfigIdentity.
    pub fn new(config_dir: PathBuf, web: bool) -> Self {
        Self { config_dir, web }
    }
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
    pub fn successor_args(&self) -> Vec<OsString> {
        let mut args = vec![
            "--config-dir".into(),
            self.config_dir.clone().into_os_string(),
            "--foreground".into(),
        ];
        if self.web {
            args.push("--web".into());
        }
        args
    }
}

/// The SAME entry instance, mask, signal reader and lifecycle drain owner.
/// Construction/getters are memory-only. The finalization leaf builds an
/// environment from these references only when the owned loop accepts a command.
pub struct ProductExitContext {
    instance: Arc<core_instance::Instance>,
    mask: Arc<core_instance::handoff::BlockedShutdownSignals>,
    signals: OperatorSignals,
    control: ControlDrainHandle,
    options: ProductOptions,
}
impl ProductExitContext {
    pub fn new(
        instance: Arc<core_instance::Instance>,
        mask: Arc<core_instance::handoff::BlockedShutdownSignals>,
        signals: OperatorSignals,
        control: ControlDrainHandle,
        options: ProductOptions,
    ) -> Self {
        Self {
            instance,
            mask,
            signals,
            control,
            options,
        }
    }
    pub fn instance(&self) -> &Arc<core_instance::Instance> {
        &self.instance
    }
    pub fn mask(&self) -> &Arc<core_instance::handoff::BlockedShutdownSignals> {
        &self.mask
    }
    pub fn signals(&self) -> &OperatorSignals {
        &self.signals
    }
    pub fn control(&self) -> &ControlDrainHandle {
        &self.control
    }
    pub fn options(&self) -> &ProductOptions {
        &self.options
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successor_argv_is_allowlisted_and_preserves_os_path_and_web_override() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"/canonical/config \xff".to_vec()));
        for web in [false, true] {
            let options = ProductOptions::new(path.clone(), web);
            let mut expected: Vec<OsString> = vec![
                "--config-dir".into(),
                path.clone().into_os_string(),
                "--foreground".into(),
            ];
            if web {
                expected.push("--web".into());
            }
            assert_eq!(options.successor_args(), expected);
        }
    }
}
