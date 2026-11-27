//! The core's `tracing` output reaches `core.log` in the log directory. This
//! file holds one test: the subscriber is process-wide.

#[test]
fn the_core_logs_to_core_log_in_the_log_directory() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("evorch").join("logs");
    evorch_core::logging::init_in(Some(logs.clone()));

    tracing::warn!(
        conn = 7,
        kind = "BrokenPipe",
        "connection task ended with an I/O error"
    );

    let text = std::fs::read_to_string(logs.join("core.log")).unwrap();
    assert!(text.contains("evorch-core logging started"), "{text}");
    assert!(
        text.contains("connection task ended with an I/O error")
            && text.contains("conn=7")
            && text.contains("BrokenPipe"),
        "{text}"
    );
}
