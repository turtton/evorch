//! Test helpers that keep tests away from the user's machine-wide state.
//!
//! Only tests depend on this crate. Production code never calls into it, so
//! the production defaults of the models.dev cache and the shell job handle
//! state stay unchanged.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// This process's private state. Statics are never dropped, so the directory
/// is removed by [`remove_state`] when the process exits instead.
static STATE: OnceLock<tempfile::TempDir> = OnceLock::new();

/// Returns this process's private state directory, creating it on first use.
///
/// The first call writes a fresh, empty models.dev cache so catalog loads
/// neither read `~/.cache/evorch` nor reach the network, and moves durable
/// shell job handle state out of `~/.config/evorch`. Shell handle state is
/// process-wide, so call this before the first shell job in the process.
/// The directory is deleted when the process exits normally.
///
/// # Panics
/// Panics when the directory cannot be prepared or shell job handles were
/// already allocated in the user's configuration directory.
pub fn isolated_state_dir() -> &'static Path {
    STATE
        .get_or_init(|| {
            let state = tempfile::Builder::new()
                .prefix("evorch-test-state-")
                .tempdir()
                .expect("create isolated test state directory");
            let catalog = state.path().join("models-dev");
            std::fs::create_dir_all(&catalog).expect("create isolated catalog directory");
            let fetched_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            std::fs::write(
                catalog.join("models-dev.json"),
                format!("{{\"fetched_at\":{fetched_at},\"api\":{{}}}}"),
            )
            .expect("write isolated catalog cache");
            tools::set_shell_job_handle_dir(state.path().join("shell-job-handles"))
                .expect("isolate shell job handle state before its first use");
            // Every test process (one per test under nextest) would otherwise
            // leave a directory behind. A failed registration only leaks it.
            // SAFETY: `remove_state` is a plain function that never unwinds
            // and only touches `STATE`, which outlives every exit handler.
            #[cfg(unix)]
            let _ = unsafe { libc::atexit(remove_state) };
            state
        })
        .path()
}

#[cfg(unix)]
extern "C" fn remove_state() {
    if let Some(state) = STATE.get() {
        let _ = std::fs::remove_dir_all(state.path());
    }
}

/// Returns a models.dev cache directory holding a fresh, empty catalog.
///
/// Pass it as the model catalog directory of a composition. It also isolates
/// shell job handle state; see [`isolated_state_dir`].
pub fn model_catalog_dir() -> PathBuf {
    isolated_state_dir().join("models-dev")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn isolated_catalog_is_fresh_and_offline() {
        let directory = super::model_catalog_dir();
        let catalog = catalog::ModelCatalog::load_or_refresh(&directory)
            .await
            .unwrap();
        assert!(catalog.find_by_model_id("claude-sonnet-4-5").is_none());
        assert!(
            catalog
                .fetched_at()
                .elapsed()
                .is_ok_and(|age| age.as_secs() < 60 * 60)
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_is_removed_when_the_process_exits() {
        const CHILD: &str = "TEST_SUPPORT_STATE_EXIT_CHILD";
        const MARKER: &str = "isolated-state-dir=";
        if std::env::var_os(CHILD).is_some() {
            let state = super::isolated_state_dir();
            assert!(state.join("models-dev/models-dev.json").is_file());
            println!("{MARKER}{}", state.display());
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::state_directory_is_removed_when_the_process_exits",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        let state = stdout
            .lines()
            .find_map(|line| line.strip_prefix(MARKER))
            .unwrap_or_else(|| panic!("child did not report its state: {stdout}"));
        assert!(!std::path::Path::new(state).exists(), "{state} remains");
    }
}
