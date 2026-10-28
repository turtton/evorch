//! The worker sandbox hides everything but its own files (ADR 0039). Needs
//! bubblewrap with user namespaces: `cargo test -p sandbox -- --ignored`.

use std::path::PathBuf;

use sandbox::{CommandSpec, MinimalSandbox, Sandbox};

fn shell() -> PathBuf {
    PathBuf::from("/bin/sh").canonicalize().unwrap()
}

fn run_in_sandbox(script: &str, forward: &[&str]) -> (String, MinimalSandbox) {
    let shell = shell();
    let sandbox =
        MinimalSandbox::detect(&shell, forward).expect("bubblewrap and the shell's libraries");
    let wrapped = sandbox
        .wrap(CommandSpec {
            program: shell.to_string_lossy().into_owned(),
            args: vec!["-c".into(), script.into()],
            cwd: None,
            extra_env: Vec::new(),
        })
        .unwrap();
    let output = wrapped.to_command().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (String::from_utf8(output.stdout).unwrap(), sandbox)
}

#[test]
#[ignore = "requires bubblewrap"]
fn the_program_sees_no_host_secret_workspace_or_environment() {
    let host = tempfile::tempdir().unwrap();
    let credential = host.path().join("credential-sentinel");
    std::fs::write(&credential, "TOP-SECRET-FILE").unwrap();
    // SAFETY: this ignored test runs alone in its process.
    unsafe {
        std::env::set_var("EVORCH_SENTINEL_SECRET", "TOP-SECRET-ENV");
        std::env::set_var("LD_LIBRARY_PATH", "/lib-from-host");
    }
    let script = format!(
        "export -p; echo ---; for entry in /* /tmp/*; do echo \"$entry\"; done; echo ---; \
         if [ -e '{credential}' ]; then echo FILE-VISIBLE; fi; \
         if [ -e \"$HOME\" ] && [ -n \"$HOME\" ]; then echo HOME-VISIBLE; fi; \
         read -r line < /proc/net/dev && echo \"$line\"; \
         while read -r name rest; do echo \"iface:$name\"; done < /proc/net/dev",
        credential = credential.display()
    );
    let (output, sandbox) = run_in_sandbox(&script, &["LD_LIBRARY_PATH"]);

    let (environment, rest) = output.split_once("---\n").unwrap();
    let (listing, probes) = rest.split_once("---\n").unwrap();
    assert!(!output.contains("TOP-SECRET"), "{output}");
    assert!(
        !probes.contains("FILE-VISIBLE") && !probes.contains("HOME-VISIBLE"),
        "{probes}"
    );
    for forbidden in [
        "HOME",
        "PATH",
        "TERM",
        "LANG",
        "LC_ALL",
        "EVORCH_SENTINEL_SECRET",
    ] {
        assert!(
            !environment.contains(&format!("declare -x {forbidden}")),
            "{forbidden} in {environment}"
        );
    }
    assert!(environment.contains("declare -x LD_LIBRARY_PATH=\"/lib-from-host\""));

    // Only the directories holding mounted files, plus the private tmpfs, /dev and /proc.
    let mut allowed: Vec<String> = ["/dev", "/proc", "/tmp"].map(String::from).into();
    allowed.extend(sandbox.mounts().iter().filter_map(|path| {
        path.components()
            .nth(1)
            .map(|first| format!("/{}", first.as_os_str().to_string_lossy()))
    }));
    for entry in listing.lines().filter(|entry| !entry.starts_with("/tmp/")) {
        assert!(
            allowed.iter().any(|top| top == entry),
            "unexpected {entry} (allowed {allowed:?})"
        );
    }
    assert!(
        listing
            .lines()
            .all(|entry| !entry.starts_with("/tmp/") || entry == "/tmp/*"),
        "tmpfs is empty: {listing}"
    );
    // No network: only the loopback interface exists, and it is down.
    let interfaces: Vec<_> = probes
        .lines()
        .filter(|line| line.starts_with("iface:"))
        .collect();
    assert!(
        interfaces
            .iter()
            .all(|line| line.contains("Inter-") || line.contains("face") || line.contains("lo:")),
        "{interfaces:?}"
    );
}
