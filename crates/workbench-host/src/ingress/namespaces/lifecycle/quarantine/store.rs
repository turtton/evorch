//! The blocking storage seam. Calls run on the admin reader worker, never on a
//! transport or the coordinator.
use storage::recovery::quarantine::{DiscardReport, DiscardRequest, QuarantineSnapshot};
use storage::{Database, StorageConfig, StorageError, StorageHandle};

pub trait QuarantineStore: Send + Sync + 'static {
    /// At most `limit` quarantined graphs, newest first.
    fn snapshot(&self, limit: usize) -> Result<QuarantineSnapshot, StorageError>;
    /// One atomic, idempotent discard. An `Err` means the answer is unknown.
    fn discard(&self, request: DiscardRequest) -> Result<DiscardReport, StorageError>;
}

/// Reads on a fresh read-only connection and writes through the single writer.
pub struct StorageQuarantineStore {
    config: StorageConfig,
    writer: StorageHandle,
}
impl StorageQuarantineStore {
    pub fn new(config: StorageConfig, writer: StorageHandle) -> Self {
        Self { config, writer }
    }
}
impl QuarantineStore for StorageQuarantineStore {
    fn snapshot(&self, limit: usize) -> Result<QuarantineSnapshot, StorageError> {
        Database::read_quarantine_snapshot(&self.config, limit)
    }
    fn discard(&self, request: DiscardRequest) -> Result<DiscardReport, StorageError> {
        self.writer.discard_quarantined_graph(request)
    }
}
