use agents::Role;
use runtime::{NetworkAccessDecision, judge_web_network_access};
use sandbox::PolicyDecision;

#[test]
fn composed_runtime_uses_web_tool_config_and_live_updates() {
    let dir = tempfile::tempdir().expect("temp");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let config = config::Config {
        sandbox: config::SandboxConfig {
            web_tools_enabled: false,
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
    assert!(!runtime.web_tools_enabled());
    runtime.set_web_tools_enabled(true);
    assert!(runtime.web_tools_enabled());

    // Enabling the Web tools does not grant unreviewed access to arbitrary URLs.
    let policy = runtime.execution_policy(Role::WebResearcher);
    assert!(matches!(
        judge_web_network_access(
            &policy.capabilities,
            &policy.role_name,
            "web_fetch",
            PolicyDecision::AutoAllow,
            runtime.web_tools_enabled(),
        ),
        NetworkAccessDecision::Ask { .. },
    ));
    runtime.set_web_tools_enabled(false);
    assert!(!runtime.web_tools_enabled());
}
