//! Bounded facade for local clients: never open the DB to diagnose boot failure.
//!
//! * [`connect`]: connect to a running core, never start one.
//! * [`reconnect`]: wait for the successor of a planned restart (connect-only).
#[path = "handoff_diagnostics/connect.rs"]
pub mod connect;
#[path = "handoff_diagnostics/reconnect.rs"]
pub mod reconnect;
#[cfg(test)]
#[path = "handoff_diagnostics/reconnect_tests.rs"]
mod reconnect_tests;
use core_instance::{ConfigIdentity, handoff::receipt::RestartBootReceipt};

#[derive(Debug, PartialEq, Eq)]
pub enum BootDiagnostic {
    Missing,
    DiagnosticUnavailable,
    Current(RestartBootReceipt),
    Historical(RestartBootReceipt),
}

pub fn read_boot_diagnostic(
    config: &ConfigIdentity,
    transaction_id: &str,
    predecessor_epoch: u64,
    successor_epoch: u64,
) -> BootDiagnostic {
    match config.read_restart_receipt() {
        Ok(Some(receipt))
            if receipt.transaction_id == transaction_id
                && receipt.predecessor_epoch == predecessor_epoch
                && receipt.successor_epoch == successor_epoch =>
        {
            BootDiagnostic::Current(receipt)
        }
        Ok(Some(receipt)) => BootDiagnostic::Historical(receipt),
        Ok(None) => BootDiagnostic::Missing,
        Err(_) => BootDiagnostic::DiagnosticUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_reader_separates_missing_current_historical_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let config = ConfigIdentity::new(dir.path()).unwrap();
        assert_eq!(
            read_boot_diagnostic(&config, "tx", 1, 2),
            BootDiagnostic::Missing
        );
        let receipt = RestartBootReceipt::restarting(&config, "tx".into(), 1, 2).unwrap();
        config.write_restart_receipt(&receipt).unwrap();
        assert_eq!(
            read_boot_diagnostic(&config, "tx", 1, 2),
            BootDiagnostic::Current(receipt.clone())
        );
        assert_eq!(
            read_boot_diagnostic(&config, "old", 1, 2),
            BootDiagnostic::Historical(receipt.clone())
        );
        assert_eq!(
            read_boot_diagnostic(&config, "tx", 2, 3),
            BootDiagnostic::Historical(receipt)
        );
        std::fs::write(config.restart_receipt_path(), b"{").unwrap();
        assert_eq!(
            read_boot_diagnostic(&config, "tx", 1, 2),
            BootDiagnostic::DiagnosticUnavailable
        );
    }
}
