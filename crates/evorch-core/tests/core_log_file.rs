//! The core's `tracing` output reaches its log file, but only the targets that
//! explain a connection: a crate that logs what it was given (an external
//! checker's stderr, which can hold credentials) never reaches the file. This
//! file holds one test: the subscriber is process-wide.

#[test]
fn the_core_saves_connection_diagnostics_and_nothing_a_tool_logged() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("evorch").join("logs");
    evorch_core::logging::init_in(Some(logs.clone()));
    evorch_core::logging::start();

    tracing::warn!(
        target: "evorch_core::connection_log",
        conn = 7,
        kind = "BrokenPipe",
        "connection task ended with an I/O error"
    );
    tracing::warn!(
        target: "tools::post_edit",
        stderr = "token=sk-live-0123456789",
        "external checker failed"
    );
    tracing::error!(target: "providers::wire", "Authorization: Bearer sk-live-0123456789");
    evorch_core::logging::flush_unbounded();

    let path = logs.join(format!("core-{}.log", std::process::id()));
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains("evorch-core logging started"), "{text}");
    assert!(
        text.contains("connection task ended with an I/O error")
            && text.contains("conn=7")
            && text.contains("BrokenPipe"),
        "{text}"
    );
    assert!(
        !text.contains("sk-live") && !text.contains("external checker failed"),
        "{text}"
    );
}
