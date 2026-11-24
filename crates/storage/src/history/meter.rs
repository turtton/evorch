//! Index accounting is independent of the legacy scanner's row/body meters.
#[cfg(feature = "test-support")]
use rusqlite::StatementStatus;
use rusqlite::{Connection, Statement};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCounts {
    pub payload_decodes: u64,
    pub source_rows: u64,
    pub source_references: u64,
    pub cut_headers: u64,
    pub message_headers: u64,
    pub receipt_headers: u64,
    pub fingerprint_bytes: u64,
    pub merge_candidates: u64,
    pub vm_steps: u64,
    pub fullscan_steps: u64,
    pub sorts: u64,
}

pub(crate) fn record(conn: &Connection, update: impl FnOnce(&mut HistoryCounts)) {
    #[cfg(feature = "test-support")]
    support::record(conn, update);
    #[cfg(not(feature = "test-support"))]
    let _ = (conn, update);
}

pub(crate) fn statement(conn: &Connection, statement: &Statement<'_>) {
    #[cfg(feature = "test-support")]
    {
        // Cached statements may have executed before a meter was attached.
        let vm = statement.reset_status(StatementStatus::VmStep) as u64;
        let scans = statement.reset_status(StatementStatus::FullscanStep) as u64;
        let sorts = statement.reset_status(StatementStatus::Sort) as u64;
        record(conn, |c| {
            c.vm_steps += vm;
            c.fullscan_steps += scans;
            c.sorts += sorts;
        });
    }
    #[cfg(not(feature = "test-support"))]
    let _ = (conn, statement);
}

#[cfg(feature = "test-support")]
pub(crate) mod support {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock, Weak},
    };

    #[derive(Debug, Default)]
    pub struct HistoryMeter(Mutex<HistoryCounts>);
    impl HistoryMeter {
        #[must_use]
        pub fn counts(&self) -> HistoryCounts {
            *self.0.lock().unwrap()
        }
    }
    fn meters() -> &'static Mutex<HashMap<String, Weak<HistoryMeter>>> {
        static METERS: OnceLock<Mutex<HashMap<String, Weak<HistoryMeter>>>> = OnceLock::new();
        METERS.get_or_init(Mutex::default)
    }
    /// Count index reads on every connection to the same file-backed store.
    ///
    /// # Panics
    /// Requires a file-backed database.
    #[must_use]
    pub fn meter_history(db: &crate::Database) -> Arc<HistoryMeter> {
        let path = db
            .conn
            .path()
            .filter(|p| !p.is_empty())
            .expect("file-backed database");
        let mut meters = meters().lock().unwrap();
        meters.retain(|_, meter| meter.strong_count() > 0);
        if let Some(existing) = meters.get(path).and_then(Weak::upgrade) {
            return existing;
        }
        let meter = Arc::new(HistoryMeter::default());
        meters.insert(path.to_owned(), Arc::downgrade(&meter));
        meter
    }
    pub(super) fn record(conn: &Connection, update: impl FnOnce(&mut HistoryCounts)) {
        let meter = conn
            .path()
            .and_then(|p| meters().lock().unwrap().get(p).and_then(Weak::upgrade));
        if let Some(meter) = meter {
            update(&mut meter.0.lock().unwrap());
        }
    }
}
