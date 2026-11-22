//! The `evorch run` binary is a client of the core (ADR 0045 §10): it never
//! opens the configuration database or builds a runtime. What it does once it
//! meets a core (the invocation, Ctrl-C, questions, exit codes) is tested
//! against a real core in `crates/core-client-e2e/tests/run_cli.rs`.
//!
//! A command line the CLI refuses is refused before it looks for a core, so
//! these runs start no core and reach no provider.

use std::process::{Command, Output};

fn run(args: &[&str], home: &std::path::Path) -> Output {
    Command::new(std::env::var("CARGO_BIN_EXE_evorch").expect("CARGO_BIN_EXE_evorch"))
        .arg("run")
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .output()
        .unwrap()
}

const BASE: [&str; 6] = ["--project", ".", "--role", "worker", "--prompt", "hello"];

#[test]
fn options_the_core_owns_are_refused_before_a_core_is_looked_for() {
    for (flag, value, shown) in [
        ("--db", "events.db", "--db is not accepted"),
        (
            "--web-tools-enabled",
            "true",
            "--web-tools-enabled is not accepted",
        ),
        (
            "--run-js-enabled",
            "false",
            "--run-js-enabled is not accepted",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let mut args = BASE.to_vec();
        args.extend([flag, value]);

        let output = run(&args, home.path());

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{flag}: {output:?}");
        assert!(stderr.contains(shown), "{flag}: {stderr}");
        assert!(
            !home.path().join("events.db").exists()
                && !home.path().join("config").exists()
                && !home.path().join("run").exists(),
            "{flag}: a refused command line created a database or started a core"
        );
    }
}

#[test]
fn a_command_line_without_a_prompt_is_the_usage() {
    let home = tempfile::tempdir().unwrap();

    let output = run(&["--project", ".", "--role", "worker"], home.path());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(stderr.contains("--no-wait"), "the usage names it: {stderr}");
    assert!(!home.path().join("run").exists());
}
