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

/// Runs `eval run --output <output>` against an evaluation directory under
/// `home` and asserts the target is refused before any core is looked for.
fn assert_output_refused(output: &str, home: &std::path::Path, config: &std::path::Path) {
    let result = eval(
        &[
            "run",
            "--thread",
            "thread-1",
            "--user-config",
            config.to_str().unwrap(),
            "--output",
            output,
        ],
        home,
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(1), "{output}: {result:?}");
    assert!(stderr.contains(output), "{output}: {stderr}");
    assert!(
        !home.join("run").exists(),
        "{output}: a refused target started a core"
    );
}

#[test]
fn an_output_that_leaves_the_evaluation_directory_is_refused_before_a_core_starts() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("user-config");
    let root = config.join("eval");
    let outside = home.path().join("elsewhere");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

    let dotdot = format!("{}/../elsewhere", root.display());
    let outside_absolute = outside.join("run-a").display().to_string();
    let mut refused = vec![
        "../run-a".to_owned(),
        "sub/run-a".to_owned(),
        dotdot,
        outside_absolute,
    ];
    #[cfg(unix)]
    {
        refused.push(root.join("escape").display().to_string());
        refused.push(root.join("escape").join("run-a").display().to_string());
        refused.push("escape".to_owned());
    }
    for output in &refused {
        assert_output_refused(output, home.path(), &config);
    }
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
}
