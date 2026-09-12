//! run の永続化 writer と読み取り接続。

use std::sync::Mutex;

use storage::{
    Database, RunContextRecord, RunLedgerEntry, StorageConfig, StorageError, StorageHandle,
};

use crate::RunId;

pub struct RunStore {
    pub(crate) handle: StorageHandle,
    database: Mutex<Database>,
}

impl RunStore {
    /// 同じ DB を使用する writer handle と読み取り接続を束ねる。
    ///
    /// # Errors
    /// DB を開けない場合に storage のエラーを返す。
    pub fn open(config: &StorageConfig, handle: StorageHandle) -> Result<Self, StorageError> {
        Ok(Self {
            handle,
            database: Mutex::new(Database::open(config)?),
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
