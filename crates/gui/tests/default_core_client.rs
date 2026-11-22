//! The default launch of `evorch-gui` (stage 19a): with no option it is a client of the
//! single core, and a core that cannot be started ends the process with its reason instead
//! of opening an embedded runtime on the same configuration directory.
//!
//! The binary is copied beside nothing (no `evorch-core`) and run with no display, so the
//! outcome is decided before any window exists and the test ends in bounded time.
#![cfg(target_os = "linux")]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const LIMIT: Duration = Duration::from_secs(60);

fn finish(mut child: Child) -> Output {
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "evorch-gui did not end within {LIMIT:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Install {
    _dir: tempfile::TempDir,
    exe: PathBuf,
    config: PathBuf,
}

fn install() -> Install {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let exe = bin.join("evorch-gui");
    fs::copy(std::env::var_os("CARGO_BIN_EXE_evorch-gui").unwrap(), &exe).unwrap();
    test_support::guard_bundle::bind_copy(&exe, "gui").unwrap();
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    Install {
        _dir: dir,
        exe,
        config,
    }
}

fn launch(install: &Install, args: &[&str], config_env: Option<&Path>) -> Output {
    let mut command = Command::new(&install.exe);
    command
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("EVORCH_CONFIG_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(config) = config_env {
        command.env("EVORCH_CONFIG_DIR", config);
    }
    finish(command.spawn().unwrap())
}

/// What an embedded runtime or an owner would leave in the configuration directory.
fn writer_files(config: &Path) -> Vec<String> {
    ["evorch-events.db", "owners.db", "sidebar.json", "ui.toml"]
        .into_iter()
        .filter(|name| config.join(name).exists())
        .map(String::from)
        .collect()
}

// Given: an installation with no evorch-core beside evorch-gui
// When: it is started with no option (the config directory from the environment), and
//       with the option the 19a opt-in used
// Then: both ask for the core, fail with its reason in bounded time, and leave no
//       database or owner in the configuration directory
#[test]
fn default_launch_is_a_core_client_and_a_missing_core_does_not_fall_back() {
    let install = install();
    let config = install.config.to_str().unwrap().to_owned();
    for (args, environment) in [
        (vec![], Some(install.config.as_path())),
        (vec!["--core-client", "--config-dir", config.as_str()], None),
        (vec!["--config-dir", config.as_str()], None),
    ] {
        let output = launch(&install, &args, environment);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(
            stderr.contains("evorch-core is not available beside evorch-gui")
                && stderr.contains("does not run its own runtime"),
            "{args:?}: {stderr}"
        );
        assert_eq!(
            writer_files(&install.config),
            Vec::<String>::new(),
            "{args:?}"
        );
    }
}

// Given: the same installation
// When: an option that only the embedded runtime has is given without --legacy-embedded,
//       and --legacy-embedded is combined with the core client's --config-dir
// Then: the start is refused by name before anything is opened
#[test]
fn embedded_only_options_do_not_start_the_embedded_runtime_implicitly() {
    let install = install();
    let state = install.config.join("sidebar.json");
    let state = state.to_str().unwrap();
    let config = install.config.to_str().unwrap();
    for args in [
        vec!["--state", state],
        vec!["--legacy-embedded", "--config-dir", config],
    ] {
        let output = launch(&install, &args, None);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains("argument error"), "{args:?}: {stderr}");
        assert_eq!(
            writer_files(&install.config),
            Vec::<String>::new(),
            "{args:?}"
        );
    }
}
