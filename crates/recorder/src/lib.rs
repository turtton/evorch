//! Event and usage persistence shared by the GUI and headless runs.
//!
//! `storage_bridge` routes event bus traffic to the storage writer and records
//! the usage ledger; `pricing` resolves the prices that ledger rows are costed at.

pub mod pricing;
pub mod storage_bridge;
