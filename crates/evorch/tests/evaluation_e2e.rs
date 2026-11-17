//! The `evorch eval` binary hands run, report and rejudge to the core (ADR 0045
//! §10) and never opens the configuration database itself. What a replay does
//! once it runs in the core (preflight failure and its report, the prompt-cache
//! contract) is tested through the core in
//! `crates/evorch-core/tests/core_eval_cache.rs`.
//!
//! A command line the CLI refuses is refused before it looks for a core, so
//! these runs start no core and reach no provider.

use std::process::{Command, Output};

fn eval(args: &[&str], home: &std::path::Path) -> Output {
    Command::new(std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"))
        .arg("eval")
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .output()
        .unwrap()
}

#[test]
fn a_database_path_is_refused_because_the_core_owns_the_database() {
    let home = tempfile::tempdir().unwrap();
    let db = home.path().join("events.db");
    let output = eval(
        &["run", "--thread", "thread-1", "--db", db.to_str().unwrap()],
        home.path(),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("--db is not accepted"), "{stderr}");
    assert!(!db.exists(), "the CLI created a database");
}

#[test]
fn a_run_without_a_thread_is_refused_with_the_usage() {
    let home = tempfile::tempdir().unwrap();
    let output = eval(&["run"], home.path());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("--thread"), "{stderr}");
    assert!(
        !home.path().join("config").exists() && !home.path().join("run").exists(),
        "a refused command line touched the configuration or started a core"
    );
}
