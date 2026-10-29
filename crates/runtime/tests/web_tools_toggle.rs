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
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(dir.path().join("empty-user-config")),
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

#[test]
fn composed_runtime_applies_run_js_config_and_live_updates() {
    let dir = tempfile::tempdir().expect("temp");
    let bus = std::sync::Arc::new(event_bus::EventBus::new(32));
    let compose = |config: &config::Config| {
        runtime::compose_runtime(runtime::RuntimeComposition {
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(dir.path().join("empty-user-config")),
            config,
            bus: bus.clone(),
            executor: std::sync::Arc::new(tools::ToolExecutor::with_standard_tools(
                bus.clone(),
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
        .runtime
    };

    // Given: the default config leaves run_js off.
    let default_runtime = compose(&config::Config::default());
    assert!(!default_runtime.run_js_enabled());
    assert_eq!(
        default_runtime.run_js_settings(),
        config::RunJsConfig::default()
    );

    // When: the config enables it with custom limits.
    let configured = config::RunJsConfig {
        enabled: true,
        max_tool_calls: 7,
        ..Default::default()
    };
    let runtime = compose(&config::Config {
        tools: config::ToolsConfig {
            run_js: configured.clone(),
            ..Default::default()
        },
        ..Default::default()
    });

    // Then: the runtime holds those settings and later updates replace them.
    assert!(runtime.run_js_enabled());
    assert_eq!(runtime.run_js_settings(), configured);
    runtime.set_run_js_settings(config::RunJsConfig::default());
    assert!(!runtime.run_js_enabled());
}
