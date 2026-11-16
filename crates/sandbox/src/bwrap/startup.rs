//! Planned bootstrap cannot spawn a child until control Ready is durable.
use super::*;
use std::sync::Mutex;

pub struct PlannedBwrap {
    config: BwrapConfig,
    ledger: crate::SpawnLedger,
    ready: Mutex<Option<Arc<BwrapSandbox>>>,
}
impl PlannedBwrap {
    /// Configuration only: even a failed or accidental tool call cannot probe.
    pub fn new(config: BwrapConfig, ledger: crate::SpawnLedger) -> Self {
        Self {
            config,
            ledger,
            ready: Mutex::new(None),
        }
    }
    /// The caller must hold its Ready gate; failure leaves execution disabled.
    pub fn initialize(&self) -> Result<(), SandboxError> {
        let mut ready = self.ready.lock().unwrap();
        if ready.is_none() {
            *ready = Some(Arc::new(BwrapSandbox::detect_owned(
                self.config.clone(),
                &self.ledger,
            )?));
        }
        Ok(())
    }
    pub fn ledger(&self) -> &crate::SpawnLedger {
        &self.ledger
    }
    fn active(&self) -> Result<Arc<BwrapSandbox>, SandboxError> {
        self.ready
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| SandboxError::BwrapUnavailable {
                detail: "planned sandbox has not passed its post-Ready probe".into(),
            })
    }
}
impl crate::Sandbox for PlannedBwrap {
    fn wrap(&self, spec: crate::CommandSpec) -> Result<crate::WrappedCommand, SandboxError> {
        self.active()?.wrap(spec)
    }
    fn with_network_access(&self) -> Result<Arc<dyn crate::Sandbox>, SandboxError> {
        self.active()?.with_network_access()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn before_initialization_even_execution_attempts_cannot_probe() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = crate::SpawnLedger::new();
        let sandbox = PlannedBwrap::new(BwrapConfig::new(dir.path().into()), ledger.clone());
        assert!(
            sandbox
                .wrap(crate::CommandSpec {
                    program: "true".into(),
                    args: vec![],
                    cwd: None,
                    extra_env: vec![],
                })
                .is_err()
        );
        assert!(sandbox.with_network_access().is_err());
        assert_eq!(ledger.child_stats().registered, 0);
        ledger.close_fence();
        assert!(sandbox.initialize().is_err());
        assert_eq!(ledger.child_stats().registered, 0);
    }
}
