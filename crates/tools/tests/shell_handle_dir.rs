//! Relocating durable shell job handle state for a whole process.
//!
//! This binary holds a single test because the location is process-wide and
//! fixed by the first allocation.

use std::sync::Arc;

use event_bus::EventBus;
use tools::{ToolError, ToolExecutor, set_shell_job_handle_dir};

#[test]
fn explicit_handle_directory_receives_all_state_and_is_fixed_once_set() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("handles");
    assert!(matches!(
        set_shell_job_handle_dir("relative/handles"),
        Err(ToolError::InvalidArgs { .. })
    ));
    set_shell_job_handle_dir(&state).unwrap();
    set_shell_job_handle_dir(&state).unwrap();
    let other = home.path().join("other");
    assert!(set_shell_job_handle_dir(&other).is_err());

    let executor = ToolExecutor::new(Arc::new(EventBus::new(16)));
    executor.reserve_shell_job_handles(7).unwrap();
    let counter = std::fs::read_to_string(state.join("counter")).unwrap();
    assert!(counter.ends_with("\n7\n"), "{counter:?}");
    assert!(state.join("lock").is_file());
    assert!(!other.exists());
}
