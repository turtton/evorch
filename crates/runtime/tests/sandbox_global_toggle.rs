use agents::{NetworkAccess, Role};
use runtime::{ExecutionPolicy, SandboxNetworkMode};

#[test]
fn composed_runtime_uses_global_config_and_live_updates() {
    // Given: a configured global opt-in at the composition boundary.
    let dir = tempfile::tempdir().expect("temp");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let config = config::Config {
        sandbox: config::SandboxConfig {
            allow_network: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let runtime = runtime::compose_runtime(runtime::RuntimeComposition {
        config: &config,
        bus: bus.clone(),
        executor: std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
            bus,
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )),
        credential_store: std::sync::Arc::new(
            sandbox::credential::FileCredentialStore::open(dir.path()).expect("store"),
        ),
        env: std::sync::Arc::new(routing::MapEnv::default()),
        model_source: runtime::ModelSource::Fixed(std::sync::Arc::new(
            runtime::compose::UnconfiguredModel,
        )),
        workspace: None,
    })
    .expect("compose")
    .runtime;
    // When / Then: new policies use config, and disabling takes effect without recomposition.
    assert_eq!(
        runtime
            .execution_policy(Role::Explorer)
            .sandbox_network_mode(),
        SandboxNetworkMode::ParentNetns
    );
    assert_eq!(
        runtime
            .execution_policy(Role::Worker)
            .sandbox_network_mode(),
        SandboxNetworkMode::Unshared
    );
    runtime.set_sandbox_network(false);
    assert_eq!(
        runtime
            .execution_policy(Role::Explorer)
            .sandbox_network_mode(),
        SandboxNetworkMode::Unshared
    );
}

#[test]
#[ignore = "requires usable bwrap"]
fn bash_connectivity_matches_global_toggle_and_role() {
    // Given: a local TCP server reachable only in the parent namespace.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("workspace");
    for (role, enabled, connected) in [
        (Role::Explorer, true, true),
        (Role::Explorer, false, false),
        (Role::Worker, true, false),
    ] {
        let policy = ExecutionPolicy::for_role(role).with_sandbox_network(enabled);
        let sandbox = runtime::build_sandbox(&policy, dir.path().into()).expect("bwrap");
        // When: bash attempts a real connection using the policy-derived sandbox.
        let wrapped = sandbox
            .wrap(sandbox::CommandSpec {
                program: "bash".into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "echo hi > /dev/tcp/127.0.0.1/{}",
                        listener.local_addr().expect("address").port()
                    ),
                ],
                cwd: Some(dir.path().into()),
                extra_env: Vec::new(),
            })
            .expect("wrap");
        let result = std::process::Command::new(wrapped.program)
            .args(&wrapped.args)
            .env_clear()
            .envs(wrapped.env)
            .output()
            .expect("bash");
        // Then: both the namespace argument and actual connectivity match the decision.
        assert_eq!(
            wrapped.args.iter().any(|arg| arg == "--unshare-net"),
            !connected
        );
        assert_eq!(
            result.status.success(),
            connected,
            "{role:?}/{enabled}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn opt_in_role_gets_network_when_global_toggle_on() {
    // Given: an opt-in role and the global sandbox opt-in.
    let policy = ExecutionPolicy::for_role(Role::Explorer).with_sandbox_network(true);
    // When: resolving its subprocess namespace.
    let mode = policy.sandbox_network_mode();
    // Then: it shares the parent network.
    assert_eq!(mode, SandboxNetworkMode::ParentNetns);
}

#[test]
fn opt_in_role_stays_blocked_when_global_toggle_off() {
    // Given: the default global setting.
    let policy = ExecutionPolicy::for_role(Role::Explorer).with_sandbox_network(false);
    // When / Then: subprocess networking remains isolated.
    assert_eq!(policy.sandbox_network_mode(), SandboxNetworkMode::Unshared);
}

#[test]
fn denied_role_stays_blocked_when_global_toggle_on() {
    // Given: a hard-denied role and an enabled global toggle.
    let policy = ExecutionPolicy::for_role(Role::Worker).with_sandbox_network(true);
    assert_eq!(policy.capabilities.network, NetworkAccess::Denied);
    // When / Then: global opt-in cannot override the hard denial.
    assert_eq!(policy.sandbox_network_mode(), SandboxNetworkMode::Unshared);
}
