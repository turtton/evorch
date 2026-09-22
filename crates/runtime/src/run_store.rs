//! run の永続化 writer と読み取り接続。

use std::sync::Mutex;

use storage::{
    Database, RunContextRecord, RunLedgerEntry, StorageConfig, StorageError, StorageHandle,
};

use crate::RunId;

pub struct RunStore {
    pub(crate) handle: StorageHandle,
    database: Mutex<Database>,
    pub(crate) next_run_id: u64,
    pub(crate) restore_gate: Mutex<()>,
}

impl RunStore {
    pub(crate) fn latest_terminal_named(
        &self,
        name: &str,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .latest_terminal_run_context(name)
    }

    /// 同じ DB を使用する writer handle と読み取り接続を束ねる。
    ///
    /// # Errors
    /// DB を開けない場合に storage のエラーを返す。
    pub fn open(config: &StorageConfig, handle: StorageHandle) -> Result<Self, StorageError> {
        let database = Database::open(config)?;
        let next_run_id = database
            .max_persisted_run_id()?
            .checked_add(1)
            .ok_or_else(|| StorageError::Serialization("run ID overflow".into()))?;
        Ok(Self {
            handle,
            database: Mutex::new(database),
            next_run_id,
            restore_gate: Mutex::new(()),
        })
    }

    pub(crate) fn restore_record(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunContextRecord>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_context(&run_id.to_string())
    }

    pub(crate) fn restore_parent(
        &self,
        run_id: RunId,
    ) -> Result<Option<Option<String>>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_context_parent(&run_id.to_string())
    }

    pub(crate) fn ledger_entries(
        &self,
        run_id: RunId,
    ) -> Result<Vec<RunLedgerEntry>, StorageError> {
        self.database
            .lock()
            .map_err(|error| StorageError::Io(error.to_string()))?
            .run_ledger(&run_id.to_string())
    }
}
