//! Capture host tracing as well as provider/event output, including spawned tasks.
use std::{
    io,
    sync::{Arc, Mutex, OnceLock},
};
type Bytes = Arc<Mutex<Vec<u8>>>;
static LOG: OnceLock<Bytes> = OnceLock::new();
struct Writer(Bytes);
impl io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub fn capture() {
    LOG.get_or_init(|| {
        let bytes = Bytes::default();
        let writer = bytes.clone();
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing_subscriber::filter::LevelFilter::TRACE)
            .with_writer(move || Writer(writer.clone()))
            .init();
        bytes
    });
}
pub fn assert_redacted(secret: &str) {
    let bytes = LOG.get().unwrap().lock().unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains(secret),
        "credential reached host tracing"
    );
}
