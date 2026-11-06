//! Cross-thread accounting for decoded full-body reads through Database's read
//! APIs. File-backed databases are keyed by SQLite's database path, so the meter
//! also sees projection tasks using another thread/reader for the same store.
use rusqlite::Connection;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyReadCounts {
    /// Requested blob refs (including failures) or returned full receipt bodies.
    pub calls: u64,
    /// Successfully decoded text bytes, before any ReadBody chunking/redaction.
    pub bytes: u64,
}
#[derive(Debug, Default)]
pub struct BodyReadMeter(Mutex<BodyReadCounts>);
impl BodyReadMeter {
    #[must_use]
    pub fn counts(&self) -> BodyReadCounts {
        *self.0.lock().unwrap()
    }
}
type Meters = HashMap<String, Weak<BodyReadMeter>>;
fn meters() -> &'static Mutex<Meters> {
    static METERS: OnceLock<Mutex<Meters>> = OnceLock::new();
    METERS.get_or_init(Mutex::default)
}
/// Meter explicit blob resolution and full host-receipt reads. Deferred event
/// descriptors and bounded inline text are intentionally not full-body reads.
///
/// # Panics
/// Requires a file-backed database, so readers on other threads share its key.
#[must_use]
pub fn meter_full_body_reads(db: &crate::Database) -> Arc<BodyReadMeter> {
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
    let meter = Arc::new(BodyReadMeter::default());
    meters.insert(path.to_owned(), Arc::downgrade(&meter));
    meter
}
pub(crate) fn record(conn: &Connection, calls: u64, bytes: u64) {
    let meter = conn
        .path()
        .and_then(|p| meters().lock().unwrap().get(p).and_then(Weak::upgrade));
    if let Some(meter) = meter {
        let mut counts = meter.0.lock().unwrap();
        counts.calls = counts.calls.saturating_add(calls);
        counts.bytes = counts.bytes.saturating_add(bytes);
    }
}
