//! Default process isolation and the command-specific network sandbox variant.

use agents::Role;
use runtime::{ExecutionPolicy, build_sandbox};
use sandbox::CommandSpec;
use std::{net::TcpListener, process::Command};

#[test]
#[ignore = "requires usable bwrap"]
fn every_role_starts_with_an_isolated_process_network() {
    let workspace = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Oracle,
        Role::MultimodalLooker,
    ] {
        let sandbox = build_sandbox(&ExecutionPolicy::for_role(role), workspace.path().into())
            .expect("bwrap");
        let wrapped = sandbox
            .wrap(CommandSpec {
                program: "true".into(),
                args: vec![],
                cwd: None,
                extra_env: vec![],
            })
            .expect("wrap");
        assert!(
            wrapped.args.iter().any(|arg| arg == "--unshare-net"),
            "{role:?}"
        );
    }
}

#[test]
#[ignore = "requires usable bwrap"]
fn command_network_variant_connects_without_changing_the_default_sandbox() {
    let workspace = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let sandbox = build_sandbox(
        &ExecutionPolicy::for_role(Role::Worker),
        workspace.path().into(),
    )
    .expect("bwrap");
    let network = sandbox.with_network_access().expect("network variant");
    let command = CommandSpec {
        program: "bash".into(),
        args: vec![
            "-c".into(),
            format!(
                "echo hi > /dev/tcp/127.0.0.1/{}",
                listener.local_addr().unwrap().port()
            ),
        ],
        cwd: Some(workspace.path().into()),
        extra_env: vec![],
    };
    for (sandbox, connected) in [
        (sandbox.as_ref(), false),
        (network.as_ref(), true),
        (sandbox.as_ref(), false),
    ] {
        let wrapped = sandbox.wrap(command.clone()).expect("wrap");
        assert_eq!(
            wrapped.args.iter().any(|arg| arg == "--unshare-net"),
            !connected
        );
        let output = Command::new(wrapped.program)
            .args(wrapped.args)
            .env_clear()
            .envs(wrapped.env)
            .current_dir(workspace.path())
            .output()
            .expect("command");
        assert_eq!(
            output.status.success(),
            connected,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
