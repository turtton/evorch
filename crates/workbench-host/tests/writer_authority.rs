//! Real core composition, with only model and sandbox replaced. No clients connect.
mod common;
use std::sync::Arc;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};
const DAY: i64 = storage::thread_retention::NANOS_PER_DAY;
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
fn options(dir: &test_socket_dir::SplitDir) -> BootstrapOptions {
    BootstrapOptions {
        config_dir: dir.path().into(),
        ownership_root: dir.sockets().into(),
        project_root: dir.path().into(),
    }
}
async fn open(options: BootstrapOptions) -> BootstrappedHost {
    std::fs::create_dir_all(&options.config_dir).unwrap();
    let mut core = BootstrappedHost::open_with_test_seams(
        options,
        TestSeams {
            model: Arc::new(common::HeldModel::default()),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    assert!(core.tick_retention_for_test(0).is_err());
    core.enable_retention_producers_for_test().unwrap();
    core
}
fn seed(options: &BootstrapOptions) -> SidebarStore {
    let store = SidebarStore::new(options.config_dir.join("sidebar.json"));
    let mut doc = SidebarDocument::default();
    doc.sidebar
        .add_project(ProjectId::new("project"), "project", &options.project_root)
        .unwrap();
    let mut thread =
        ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("project"), "thread");
    thread.created_at = 0;
    thread.updated_at = 0;
    doc.sidebar.threads.push(thread);
    doc.sidebar.refresh_derived();
    store.write(&store.lock().unwrap(), &doc).unwrap();
    store
}
fn archived(store: &SidebarStore) -> bool {
    store.read_unlocked().unwrap().sidebar.resolve_states()[&ThreadId::new("thread")].archived
}
#[tokio::test]
async fn single_core_bootstrap_archives_and_binds_without_a_client_and_inspection_still_works() {
    let dir = test_socket_dir::SplitDir::new();
    let options = options(&dir);
    let mut core = open(options.clone()).await;
    let store = seed(&options);
    assert!(!archived(&store));
    let before = core.tick_retention_for_test(7 * DAY - 1).unwrap();
    assert_eq!(before.published, 0, "{before:?}");
    let scan = core.tick_retention_for_test(7 * DAY).unwrap();
    assert!(scan.holds.contains_key("ScanComplete"), "{scan:?}");
    let after = core.tick_retention_for_test(7 * DAY).unwrap();
    assert_eq!(after.published, 1, "{after:?}");
    assert!(!after.holds.contains_key("AuthorityUnavailable"));
    assert!(after.failures.is_empty(), "{after:?}");
    assert!(archived(&store));
    let config = storage::StorageConfig {
        db_path: options.config_dir.join("evorch-events.db"),
        ..Default::default()
    };
    assert!(storage::Database::open_for_inspection(&config.db_path).is_ok());
    assert!(storage::Storage::open(config.clone()).is_err());
    let db = storage::Database::open(&config).unwrap();
    assert_eq!(
        runtime::thread_retention::binding::check_binding(
            &db,
            runtime::thread_retention::binding::BindingPaths {
                ownership_root: &options.ownership_root,
                sidebar_path: store.path()
            }
        )
        .unwrap(),
        runtime::thread_retention::binding::BindingStatus::Bound
    );
    core.shutdown().await.unwrap();
    drop(core);
    assert!(storage::Storage::open(config).is_ok());
}
#[tokio::test]
async fn second_database_same_root_starts_but_never_promotes_and_its_tick_touches_no_locks() {
    let dir = test_socket_dir::SplitDir::new();
    let first_options = options(&dir);
    let mut first = open(first_options.clone()).await;
    let first_store = seed(&first_options);
    let mut second_options = first_options.clone();
    second_options.config_dir = dir.path().join("second");
    let mut second = open(second_options.clone()).await;
    let second_store = seed(&second_options);
    // Authority is checked before the host/sidebar locks, even when both are held.
    let host_lock = second.host.lock().unwrap();
    let sidebar_lock = second_store.lock().unwrap();
    let held = second.tick_retention_for_test(8 * DAY).unwrap();
    assert_eq!(held.holds.get("AuthorityUnavailable"), Some(&1));
    assert_eq!(held.published, 0);
    assert_eq!(held.attachment_calls, 0);
    drop(sidebar_lock);
    drop(host_lock);
    assert!(!archived(&second_store));
    assert_eq!(first.tick_retention_for_test(8 * DAY).unwrap().published, 1);
    assert!(archived(&first_store));
    first.shutdown().await.unwrap();
    drop(first);
    assert_eq!(
        second
            .tick_retention_for_test(8 * DAY)
            .unwrap()
            .holds
            .get("AuthorityUnavailable"),
        Some(&1)
    );
    second.shutdown().await.unwrap();
}
#[tokio::test]
async fn binding_mismatch_keeps_archive_but_suppresses_all_automatic_root_operations() {
    let dir = test_socket_dir::SplitDir::new();
    let options = options(&dir);
    std::fs::write(
        options
            .ownership_root
            .join(runtime::thread_retention::binding::ROOT_BINDING_FILE),
        r#"{"db_id":"other-db","db_path":"elsewhere","bound_at_ns":0}"#,
    )
    .unwrap();
    let mut core = open(options.clone()).await;
    let store = seed(&options);
    let report = core.tick_retention_for_test(8 * DAY).unwrap();
    assert_eq!(report.published, 1, "{report:?}");
    assert!(archived(&store));
    assert_eq!(report.attachment_calls, 0);
    assert_eq!(report.scratch_hints, 0);
    assert!(report.holds.contains_key("BindingMismatch"));
    core.shutdown().await.unwrap();
}
#[tokio::test]
async fn failed_core_startup_releases_both_writer_leases() {
    let dir = test_socket_dir::SplitDir::new();
    let options = options(&dir);
    // Invalid SQLite bytes fail after writer acquisition, before host activation.
    std::fs::write(options.config_dir.join("evorch-events.db"), b"not sqlite").unwrap();
    assert!(
        BootstrappedHost::open_with_test_seams(
            options.clone(),
            TestSeams {
                model: Arc::new(common::HeldModel::default()),
                sandbox: Arc::new(Direct)
            }
        )
        .await
        .is_err()
    );
    let leases = storage::WriterLeases::acquire(
        &options.config_dir.join("evorch-events.db"),
        &options.ownership_root,
        std::time::Duration::ZERO,
    )
    .unwrap();
    assert!(leases.authority().is_some());
}
