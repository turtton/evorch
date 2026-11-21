//! The single place that publishes management.
//!
//! `MANAGEMENT_PUBLISHED` is the only product switch: there is no environment
//! variable or configuration key. Tests that assemble an in-process host call
//! `AdminService::enable_management` themselves.
use crate::backend::HostBackend;
use workbench_host::admin::{AdminService, InstalledFacades};

/// Flipped to `true` only by the change that publishes management.
const MANAGEMENT_PUBLISHED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    /// The switch is off: management stays refused and unadvertised.
    Off,
    /// The switch is on but a leaf is missing or the platform cannot run the
    /// effects: still refused and unadvertised.
    Blocked(InstalledFacades),
    Published,
}

pub(super) fn publish(admin: &AdminService, backend: &HostBackend) -> Publication {
    publish_with(
        MANAGEMENT_PUBLISHED,
        cfg!(target_os = "linux"),
        admin,
        backend,
    )
}

fn publish_with(
    published: bool,
    linux: bool,
    admin: &AdminService,
    backend: &HostBackend,
) -> Publication {
    if !published {
        return Publication::Off;
    }
    let installed = admin.installed_facades();
    if !linux || !installed.all() {
        return Publication::Blocked(installed);
    }
    admin.enable_management();
    backend.advertise_management();
    Publication::Published
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use workbench_authority::IngressContext;
    use workbench_host::{
        admin::{
            AdminFacade, AdminInspection, AdminIntent, AdminIo, AdminQuery, AdminView,
            CommitAdmission, VerifiedConnection,
        },
        bootstrap::{BootstrapOptions, BootstrappedHost},
    };
    use workbench_protocol::{
        envelope::{Provenance, Surface},
        errors::WorkbenchError,
        handshake::Capability,
        ids::ClientId,
    };
    use workbench_web::WorkbenchBackend;

    struct Leaf;
    impl AdminFacade for Leaf {
        fn inspect<'a>(
            &'a self,
            _: &'a AdminIntent,
            _: &'a VerifiedConnection,
        ) -> AdminIo<'a, AdminInspection> {
            unreachable!("publication never calls a leaf")
        }
        fn query<'a>(
            &'a self,
            _: &'a AdminQuery,
            _: &'a VerifiedConnection,
        ) -> AdminIo<'a, AdminView> {
            unreachable!("publication never calls a leaf")
        }
        fn commit(
            &self,
            _: AdminIntent,
            _: AdminInspection,
            _: CommitAdmission,
            _: &VerifiedConnection,
        ) -> Result<workbench_host::admin::AdminAccepted, WorkbenchError> {
            unreachable!("publication never calls a leaf")
        }
    }

    fn offered(hello: &workbench_protocol::handshake::ServerHello) -> bool {
        hello.capabilities.contains(&Capability::SafeRestart)
    }

    #[tokio::test]
    async fn publication_needs_the_switch_every_leaf_and_linux() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-publication".into());
        let admin = boot.admin.clone();

        assert_eq!(
            publish_with(false, true, &admin, &backend),
            Publication::Off
        );
        assert!(!admin.management_enabled());
        assert!(!offered(&backend.hello()));

        // A leaf is missing: nothing is enabled until all three are registered.
        admin.install_restart(Arc::new(Leaf));
        admin.install_quarantine(Arc::new(Leaf));
        let blocked = Publication::Blocked(admin.installed_facades());
        assert_eq!(publish_with(true, true, &admin, &backend), blocked);
        assert!(!admin.management_enabled() && !offered(&backend.hello()));

        admin.install_owned_graphs(Arc::new(Leaf));
        assert!(matches!(
            publish_with(true, false, &admin, &backend),
            Publication::Blocked(installed) if installed.all()
        ));
        assert!(!admin.management_enabled() && !offered(&backend.hello()));

        assert_eq!(
            publish_with(true, true, &admin, &backend),
            Publication::Published
        );
        assert!(admin.management_enabled());
        assert!(offered(&backend.hello()));
        // The Unix ingress negotiates from `hello()`, whose peers are all verified
        // local; Web peers go through `server_hello`, which withholds the capability.
        let web = IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("web-human").unwrap(),
        ));
        assert!(!offered(&backend.server_hello(&web).await.unwrap()));
        boot.shutdown().await.unwrap();
    }

    /// Read-only storage accessors change nothing: management stays refused and
    /// unadvertised with them in hand, and the handle is the host's own writer.
    #[tokio::test]
    async fn storage_accessors_do_not_open_management() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-inputs".into());
        let inputs = crate::admin_composition::Inputs::new(&boot, &backend);
        assert_eq!(*inputs.storage_config(), boot.composition.storage);
        inputs.storage_writer().barrier().unwrap();
        let _shared = inputs.storage_writer().clone();
        assert_eq!(crate::admin_composition::install(&inputs), Publication::Off);
        assert!(!boot.admin.management_enabled());
        assert!(!offered(&backend.hello()));
        let forged = Provenance::human(Surface::Cli, ClientId::new("claimed-human").unwrap());
        let refused = boot
            .admin
            .authorize(
                &IngressContext::unverified(forged),
                workbench_protocol::namespaces::lifecycle::AdminAction::Request,
            )
            .unwrap_err();
        assert_eq!(
            refused.code,
            workbench_protocol::errors::ErrorCode::UnsupportedCapability
        );
        boot.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn the_advertisement_follows_the_service_switch() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let backend = HostBackend::new(boot.host.clone(), "core-advertise".into());
        backend.advertise_management();
        assert!(
            !offered(&backend.hello()),
            "advertising alone is not enough"
        );
        boot.admin.enable_management();
        assert!(offered(&backend.hello()));
        boot.shutdown().await.unwrap();
    }
}
