//! The sealed core, not a test substitute, retains the existing exclusion layers.
#![cfg(target_os = "linux")]
#[path = "../shared-test-support/product_exec.rs"]
mod product_exec;
#[path = "../shared-test-support/public_restart_product.rs"]
mod public_restart_product;
use product_exec::{
    artifact::ProductArtifact,
    env::IsolatedEnv,
    launch::{KEY_ENV, ProductLaunch},
    wire::MockProvider,
};
use std::{os::unix::fs::symlink, path::Path};

fn rejected(
    artifact: &ProductArtifact,
    env: &IsolatedEnv,
    config: &Path,
    code: i32,
    reason: Option<&str>,
) {
    let mut other = ProductLaunch::new(artifact, env)
        .args(["--config-dir".as_ref(), config.as_os_str()])
        .start()
        .unwrap();
    let ready = other.process.wait_for_event(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .filter(|value| value["status"] == "ready")
    });
    assert!(ready.is_err(), "a competing core became ready: {ready:?}");
    let report = other.process.join();
    assert_eq!(report.status.unwrap().code(), Some(code), "{report:?}");
    assert!(
        report.orphans.is_empty() && report.unjoined_readers == 0 && !report.reaper_lost,
        "{report:?}"
    );
    if let Some(reason) = reason {
        assert!(report.stderr.contains(reason), "{report:?}");
    }
}
fn stop(mut product: product_exec::launch::ProductProcess<'_>) {
    assert!(product.process.signal(rustix::process::Signal::TERM));
    assert!(product.process.wait_for_event::<()>(|_| None).is_err());
    let report = product.process.join();
    assert!(
        report.success() && report.orphans.is_empty() && report.unjoined_readers == 0,
        "{report:?}"
    );
}
#[test]
fn a_symlink_config_alias_cannot_start_a_second_core() {
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let mock = MockProvider::scripted(vec![]);
    env.write_product_config(&mock.base_url(), KEY_ENV);
    let mut first = ProductLaunch::new(&artifact, &env).start().unwrap();
    first.wait_ready_event().unwrap();
    let alias = env.root().join("alias");
    symlink(env.config_dir(), &alias).unwrap();
    rejected(&artifact, &env, &alias, 2, None);
    stop(first);
}
#[test]
fn different_configs_cannot_write_the_same_database() {
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let mock = MockProvider::scripted(vec![]);
    env.write_product_config(&mock.base_url(), KEY_ENV);
    let mut first = ProductLaunch::new(&artifact, &env).start().unwrap();
    first.wait_ready_event().unwrap();
    let second_config = env.root().join("other-config");
    std::fs::create_dir(&second_config).unwrap();
    std::fs::copy(
        env.config_dir().join("config.toml"),
        second_config.join("config.toml"),
    )
    .unwrap();
    symlink(
        env.config_dir().join("evorch-events.db"),
        second_config.join("evorch-events.db"),
    )
    .unwrap();
    rejected(
        &artifact,
        &env,
        &second_config,
        1,
        Some("DB writer lease unavailable"),
    );
    stop(first);
}
#[test]
fn offline_apply_refuses_the_database_of_a_ready_core() {
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let mock = MockProvider::scripted(vec![]);
    env.write_product_config(&mock.base_url(), KEY_ENV);
    let mut first = ProductLaunch::new(&artifact, &env).start().unwrap();
    first.wait_ready_event().unwrap();
    let error = storage::maintenance::strip_task_progress_cursor(
        &env.config_dir().join("evorch-events.db"),
        &storage::maintenance::StripOptions {
            apply: true,
            no_backup: true,
            no_vacuum: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("database is in use"), "{error}");
    stop(first);
}
struct Release(mock_openai::ResponseGate);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}
#[test]
fn a_third_core_is_excluded_during_new_format_planned_exec() {
    use workbench_client::restart::{
        confirmation::confirm,
        intent::{TargetSelector, plan_restart, request_restart},
        session::RestartAdmin,
    };
    use workbench_protocol::{handshake::LocalSurface, namespaces::lifecycle::AdminId};
    let artifact = ProductArtifact::require();
    let env = IsolatedEnv::new();
    let (gate, entered) = mock_openai::ResponseGate::new();
    let _release = Release(gate.clone());
    let mock = MockProvider::scripted(vec![
        mock_openai::ScriptedResponse::text_stream("held", "mock-model", ["complete"])
            .with_gate(gate.clone()),
    ]);
    env.write_product_config(&mock.base_url(), KEY_ENV);
    let mut first = ProductLaunch::new(&artifact, &env).start().unwrap();
    let ready = first.wait_ready_event().unwrap();
    let original_pid = first.pid();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let before = runtime.block_on(async {
        let desktop = public_restart_product::connect(
            &ready.socket,
            "exclusion-desktop",
            LocalSurface::Desktop,
        )
        .await;
        desktop
            .command(public_restart_product::chat(
                "exclusion-turn",
                "hold at a safe point",
            ))
            .await
            .unwrap();
        entered.recv().unwrap();
        let cli =
            public_restart_product::connect(&ready.socket, "exclusion-cli", LocalSurface::Cli)
                .await;
        let before = cli.server_hello().instance_epoch.0;
        let admin = RestartAdmin::new(&cli);
        let binding = plan_restart(
            &admin.targets().await.unwrap(),
            &TargetSelector::Current,
            AdminId::new("exclusion-restart").unwrap(),
        )
        .unwrap();
        admin
            .send(request_restart(
                binding,
                confirm(false, true, false).unwrap(),
            ))
            .await
            .unwrap();
        let status = admin.status().await.unwrap();
        assert!(
            workbench_client::restart::intent::cancellable(status.status.phase),
            "{status:?}"
        );
        rejected(&artifact, &env, &env.config_dir(), 2, None);
        gate.release();
        before
    });
    // This contender races the capture/exec, before the test observes successor Ready.
    rejected(&artifact, &env, &env.config_dir(), 2, None);
    let successor = first.wait_ready_event().unwrap();
    assert_eq!(
        first.pid(),
        original_pid,
        "planned restart is exec, not a second core"
    );
    runtime.block_on(async {
        let cli = public_restart_product::connect(
            &successor.socket,
            "exclusion-successor",
            LocalSurface::Cli,
        )
        .await;
        assert!(cli.server_hello().instance_epoch.0 > before);
    });
    rejected(&artifact, &env, &env.config_dir(), 2, None);
    stop(first);
}
#[test]
#[ignore = "role: the reaper of one supervised family (product_exec::reaper)"]
fn f4p_family_reaper() {
    product_exec::reaper::run();
}
