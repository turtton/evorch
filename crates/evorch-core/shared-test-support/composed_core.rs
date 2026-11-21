//! In-process product owners; the caller installs the composition inside its unit tree.
use crate::{
    backend::HostBackend,
    lifecycle::{
        LifecycleDriver, OperatorSignals,
        product_exit::{ProductExitContext, ProductOptions},
    },
};
use std::sync::Arc;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};

pub struct ComposedCore {
    pub dir: test_socket_dir::SplitDir,
    pub host: BootstrappedHost,
    pub backend: Arc<HostBackend>,
    pub driver: LifecycleDriver,
    pub context: ProductExitContext,
}
impl ComposedCore {
    pub async fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let host = BootstrappedHost::open_for_core_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root,
                ownership_root: dir.sockets().join("owners"),
            },
            TestSeams {
                model: Arc::new(crate::restart::tests::restart_model::Model::default()),
                sandbox: Arc::new(crate::restart::tests::restart_model::Direct),
            },
            &crate::recovery_service::CoreGraphRecovery,
        )
        .await
        .unwrap();
        let identity = core_instance::ConfigIdentity::new(dir.path()).unwrap();
        let launch = core_instance::LaunchGuard::acquire(&identity).unwrap();
        let instance = Arc::new(core_instance::Instance::acquire(&launch, dir.sockets()).unwrap());
        let mask = crate::signal_test_entry::mask().clone();
        let signals = OperatorSignals::new(&mask).unwrap();
        let backend = Arc::new(HostBackend::new(
            host.host.clone(),
            instance.identity.clone(),
        ));
        let driver = LifecycleDriver::new(&host);
        let context = ProductExitContext::new(
            instance,
            mask,
            signals,
            driver.control_drain(),
            ProductOptions::new(identity.config_dir().into(), false),
        );
        Self {
            dir,
            host,
            backend,
            driver,
            context,
        }
    }
}
/// Process signal mask must precede Tokio threads, as in the real entry.
pub async fn masked_child() -> bool {
    if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_some() {
        return false;
    }
    let name = std::thread::current().name().unwrap().to_owned();
    let result = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env("EVORCH_MASKED_TEST_CHILD", "1")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    true
}
