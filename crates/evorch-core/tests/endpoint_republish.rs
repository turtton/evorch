use std::sync::Arc;

use core_launcher::{CoreBinary, Launcher};
use evorch_core::{
    backend::HostBackend,
    instance::{ConfigIdentity, Instance, LaunchGuard},
    unix,
};
use tokio::sync::watch;
use workbench_client::ClientConfig;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_protocol::{handshake::BuildInfo, ids::ClientId};

struct Core {
    _dir: test_socket_dir::SplitDir,
    boot: BootstrappedHost,
    backend: Arc<HostBackend>,
    identity: ConfigIdentity,
    instance: Arc<Instance>,
    server: tokio::task::JoinHandle<()>,
    _stop: watch::Sender<bool>,
}

impl Core {
    async fn start() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().join("threads"),
        })
        .await
        .unwrap();
        let identity = ConfigIdentity::new(&root.join("core")).unwrap();
        let launch = LaunchGuard::acquire(&identity).unwrap();
        let instance = Arc::new(Instance::acquire(&launch, dir.sockets()).unwrap());
        drop(launch);
        let backend = Arc::new(HostBackend::new(
            boot.host.clone(),
            instance.identity.clone(),
        ));
        let listener = instance.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let (stop, stopped) = watch::channel(false);
        let serving = backend.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(unix::connection(stream, serving.clone(), stopped.clone()));
            }
        });
        backend.publish_endpoint(instance.clone()).unwrap();
        Self {
            _dir: dir,
            boot,
            backend,
            identity,
            instance,
            server,
            _stop: stop,
        }
    }

    fn record_epoch(&self) -> u64 {
        self.identity.read_record().unwrap().unwrap().epoch
    }

    fn launcher(&self) -> Launcher {
        Launcher::with_socket(
            self.identity.clone(),
            CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
            ClientConfig::new(
                ClientId::new("endpoint-test").unwrap(),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
            self.instance.socket.clone(),
        )
        .unwrap()
    }

    async fn finish(mut self) {
        self.server.abort();
        self.boot.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_launcher_attaches_after_the_projection_invalidates() {
    // Given: a core whose record holds the epoch it booted with.
    let core = Core::start().await;
    let booted = core.backend.instance_epoch();
    assert_eq!(core.record_epoch(), booted);
    let mut published = core.backend.endpoint_published().unwrap();

    // When: the projection invalidates, advancing the hello epoch.
    core.backend.projection.invalidate();
    let current = core.backend.instance_epoch();
    assert_ne!(current, booted);

    // Then: the record follows it, and a launcher attaches without launching.
    published.wait_for(|epoch| *epoch == current).await.unwrap();
    assert_eq!(core.record_epoch(), current);
    let client = core
        .launcher()
        .connect_or_start_observed(|stage| panic!("live core must not launch: {stage:?}"))
        .await
        .unwrap();
    assert_eq!(client.server_hello().instance_epoch.0, current);
    drop(client);
    core.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_stops_republishing_before_a_handoff() {
    // Given: a core republishing its record.
    let core = Core::start().await;
    let booted = core.backend.instance_epoch();
    let mut published = core.backend.endpoint_published().unwrap();

    // When: the core starts draining for a handoff, then invalidates.
    core.backend.close();
    core.backend.projection.invalidate();
    assert_ne!(core.backend.instance_epoch(), booted);

    // Then: the republisher ends without writing, so the record keeps the old epoch.
    assert!(published.changed().await.is_err());
    assert_eq!(core.record_epoch(), booted);
    core.finish().await;
}
