//! Production owner with the executable fake helper: no CEF and no sleeps.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]
#[path = "renderer_support/mod.rs"]
mod support;
use renderer_ipc::{OpenArtifact, SessionId, TabId, Viewport};
use sandbox::{ChildKind, SpawnLedger};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use workbench_host::areas::{
    AreaTasks,
    browser::renderer::{OwnerConfig, OwnerError, Phase, RendererOwner},
};
fn request(width: u32, height: u32) -> (OpenArtifact, Vec<u8>) {
    OpenArtifact::with_files(
        SessionId(1),
        TabId(999),
        Viewport { width, height },
        30,
        30_000,
        vec![(
            "index.html".into(),
            "text/html".into(),
            b"<p>owner</p>".to_vec(),
        )],
    )
}
fn fixture(root: &std::path::Path) -> (AreaTasks, SpawnLedger, RendererOwner) {
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = SpawnLedger::new();
    let exe = std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("evorch-renderer-test-helper")
        });
    let owner = RendererOwner::new(
        &tasks,
        ledger.clone(),
        OwnerConfig::new(exe, root.to_path_buf()),
    )
    .unwrap();
    (tasks, ledger, owner)
}
async fn shutdown(tasks: AreaTasks, ledger: &SpawnLedger) {
    ledger.close_fence();
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    ledger.reap_all_children().await.unwrap();
    ledger.join_workers().await.unwrap();
    assert!(ledger.children().is_empty());
    assert_eq!(ledger.child_stats().unconfirmed, 0);
}
#[tokio::test]
async fn lazy_singleflight_four_tabs_busy_resize_and_reuse() {
    let mut tree = support::Tree::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("profiles");
    let (tasks, ledger, owner) = fixture(&root);
    assert!(!root.exists());
    assert!(ledger.children().is_empty());
    let (a, b, c, d) = tokio::join!(
        async {
            let (o, b) = request(1280, 720);
            owner.open(o, b).await
        },
        async {
            let (o, b) = request(1280, 720);
            owner.open(o, b).await
        },
        async {
            let (o, b) = request(1280, 720);
            owner.open(o, b).await
        },
        async {
            let (o, b) = request(1280, 720);
            owner.open(o, b).await
        }
    );
    let tabs = [a.unwrap(), b.unwrap(), c.unwrap(), d.unwrap()];
    tree.track(&ledger);
    assert!(tabs.iter().all(|t| t.generation == tabs[0].generation));
    assert_eq!(ledger.child_stats().registered, 1);
    assert_eq!(ledger.children()[0].kind, ChildKind::Renderer);
    let (o, b) = request(1, 1);
    assert_eq!(owner.open(o, b).await, Err(OwnerError::Busy));
    assert_eq!(
        owner
            .resize(
                tabs[0],
                Viewport {
                    width: 3840,
                    height: 2160
                }
            )
            .await,
        Err(OwnerError::Busy)
    );
    owner.close(tabs[0]).await.unwrap();
    let (o, b) = request(1, 1);
    let new = owner.open(o, b).await.unwrap();
    assert_ne!(new.id, tabs[0].id);
    assert_eq!(new.generation, tabs[0].generation);
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}
#[tokio::test]
async fn four_k_generation_stops_at_last_close_even_after_shrinking() {
    let mut tree = support::Tree::new();
    let dir = tempfile::tempdir().unwrap();
    let (tasks, ledger, owner) = fixture(&dir.path().join("profiles"));
    let (release, resident_rx) = std::sync::mpsc::channel();
    let resident = ledger
        .admit()
        .unwrap()
        .spawn_registered_worker_thread("unrelated-resident", move || resident_rx.recv().unwrap())
        .unwrap();
    let (o, b) = request(3840, 2160);
    let tab = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    let (o, b) = request(1, 1);
    assert_eq!(owner.open(o, b).await, Err(OwnerError::Busy));
    owner
        .resize(
            tab,
            Viewport {
                width: 1,
                height: 1,
            },
        )
        .await
        .unwrap();
    owner.close(tab).await.unwrap();
    assert_eq!(owner.status().phase, Phase::Stopped);
    assert!(ledger.children().is_empty());
    // Phase::Stopped means actual joins completed, not merely IPC loop exit.
    // A different resident worker must not delay this generation boundary.
    assert_eq!(
        ledger.workers().iter().map(|w| w.id).collect::<Vec<_>>(),
        vec![resident]
    );
    let (o, b) = request(1, 1);
    let next = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    assert!(next.generation > tab.generation);
    assert_eq!(owner.close(tab).await, Err(OwnerError::StaleGeneration));
    release.send(()).unwrap();
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}
fn partial_profile_delete(path: &std::path::Path) -> std::io::Result<()> {
    if path.parent().unwrap().join("block-reclaim").exists() {
        for name in ["marker", "confirmed", "owner.lock"] {
            match std::fs::remove_file(path.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "partial deletion",
        ));
    }
    std::fs::remove_dir_all(path)
}
#[tokio::test]
async fn isolated_partial_profile_deletion_does_not_disable_next_generation_and_is_retried() {
    let mut tree = support::Tree::new();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("profiles");
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = SpawnLedger::new();
    let exe = std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("evorch-renderer-test-helper")
        });
    let mut config = OwnerConfig::new(exe, root.clone());
    config.remove_profile = Some(partial_profile_delete);
    let owner = RendererOwner::new(&tasks, ledger.clone(), config).unwrap();
    let (o, b) = request(3840, 2160);
    let old = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    std::fs::write(root.join("block-reclaim"), "hold deletion").unwrap();
    owner.close(old).await.unwrap();
    assert_eq!(owner.status().phase, Phase::Stopped);
    assert!(ledger.children().is_empty());
    assert!(ledger.workers().is_empty());
    let queued = std::fs::read_dir(&root)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".reclaim-"))
        .unwrap()
        .path();
    assert!(queued.exists());
    for name in ["marker", "confirmed", "owner.lock"] {
        assert!(!queued.join(name).exists());
    }
    let (o, b) = request(1, 1);
    let next = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    assert!(next.generation > old.generation);
    assert!(queued.exists()); // Startup retried, but deletion STILL fails.
    assert_eq!(ledger.child_stats().reaped, 1);
    std::fs::remove_file(root.join("block-reclaim")).unwrap();
    owner.stop().await.unwrap(); // Reclamation also retries older queue entries.
    assert!(!queued.exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}

#[tokio::test]
async fn held_hello_does_not_hold_admission_and_shutdown_reclaims_every_worker() {
    let mut tree = support::Tree::new();
    let dir = test_socket_dir::SocketDir::new();
    let root = dir.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(root.join("fixture-mode"), "gate-hello").unwrap();
    let listener = tokio::net::UnixListener::bind(root.join("gate.sock")).unwrap();
    let (tasks, ledger, owner) = fixture(&root);
    let (o, b) = request(1, 1);
    let open = owner.open(o, b);
    tokio::pin!(open);
    let mut gate = tokio::select! { _=&mut open=>panic!("Hello must wait"), accepted=listener.accept()=>accepted.unwrap().0 };
    let mut ready = [0];
    gate.read_exact(&mut ready).await.unwrap();
    tree.track(&ledger);
    // close_fence returns while Hello is blocked: Admission cannot span Hello.
    ledger.close_fence();
    assert_eq!(ledger.children().len(), 1);
    tasks.request_stop();
    assert!(open.await.is_err());
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}
#[tokio::test(start_paused = true)]
async fn idle_deadline_and_reopen_are_serialized_without_new_spawn() {
    let mut tree = support::Tree::new();
    let dir = tempfile::tempdir().unwrap();
    let (tasks, ledger, owner) = fixture(&dir.path().join("profiles"));
    let (o, b) = request(1, 1);
    let tab = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    owner.close(tab).await.unwrap();
    assert_eq!(owner.status().phase, Phase::Idle);
    tokio::time::advance(std::time::Duration::from_secs(29)).await;
    let (o, b) = request(1, 1);
    let tab2 = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    assert_eq!(tab.generation, tab2.generation);
    owner.close(tab2).await.unwrap();
    let mut status = owner.subscribe();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    status
        .wait_for(|s| s.phase == Phase::Stopped)
        .await
        .unwrap();
    assert!(ledger.children().is_empty());
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}

#[tokio::test]
async fn open_during_stopping_waits_for_old_group_reap_and_actual_worker_joins() {
    let mut tree = support::Tree::new();
    let dir = test_socket_dir::SocketDir::new();
    let root = dir.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(root.join("fixture-mode"), "gate-shutdown").unwrap();
    let listener = tokio::net::UnixListener::bind(root.join("gate.sock")).unwrap();
    let (tasks, ledger, owner) = fixture(&root);
    let (o, b) = request(1, 1);
    let old = owner.open(o, b).await.unwrap();
    tree.track(&ledger);
    let old_workers: Vec<_> = ledger.workers().iter().map(|w| w.id).collect();
    let stop = owner.stop();
    tokio::pin!(stop);
    let mut gate = tokio::select! {
        result = &mut stop => panic!("Shutdown must wait for its event: {result:?}"),
        accepted = listener.accept() => accepted.unwrap().0,
    };
    gate.read_exact(&mut [0]).await.unwrap();
    assert_eq!(owner.status().phase, Phase::Stopping);
    let (o, b) = request(1, 1);
    let reopen = owner.open(o, b);
    tokio::pin!(reopen);
    // Poll once to enqueue open while Shutdown is held, without sleeps/time
    // assertions. The old process and its workers must still be the sole owner.
    tokio::select! { biased;
        result = &mut reopen => panic!("open completed before old reclamation: {result:?}"),
        _ = std::future::ready(()) => {}
    }
    assert_eq!(owner.status().phase, Phase::Stopping);
    assert_eq!(ledger.child_stats().registered, 1);
    assert_eq!(ledger.children().len(), 1);
    assert_eq!(
        ledger.workers().iter().map(|w| w.id).collect::<Vec<_>>(),
        old_workers
    );
    std::fs::remove_file(root.join("fixture-mode")).unwrap();
    gate.write_all(b"R").await.unwrap();
    let (stopped, next) = tokio::join!(&mut stop, &mut reopen);
    stopped.unwrap();
    let next = next.unwrap();
    tree.track(&ledger);
    assert!(next.generation > old.generation);
    assert_eq!(ledger.child_stats().reaped, 1);
    assert_eq!(ledger.child_stats().registered, 2);
    assert!(
        ledger
            .workers()
            .iter()
            .all(|w| !old_workers.contains(&w.id))
    );
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}

#[tokio::test]
async fn failed_hello_remains_owned_and_fenced_spawn_never_starts() {
    let mut tree = support::Tree::new();
    let dir = test_socket_dir::SocketDir::new();
    let root = dir.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(root.join("fixture-mode"), "fail-hello").unwrap();
    let listener = tokio::net::UnixListener::bind(root.join("gate.sock")).unwrap();
    let (tasks, ledger, owner) = fixture(&root);
    let (o, b) = request(1, 1);
    let open = owner.open(o, b);
    tokio::pin!(open);
    let mut gate = tokio::select! {_=&mut open=>panic!("must reach Hello gate"),accepted=listener.accept()=>accepted.unwrap().0};
    let mut ready = [0];
    gate.read_exact(&mut ready).await.unwrap();
    tree.track(&ledger);
    gate.write_all(b"F").await.unwrap();
    assert!(open.await.is_err());
    assert!(ledger.children().is_empty());
    assert_eq!(ledger.child_stats().reaped, 1);
    let (o, b) = request(1, 1);
    assert_eq!(owner.open(o, b).await, Err(OwnerError::Busy));
    shutdown(tasks, &ledger).await;
    let dir = tempfile::tempdir().unwrap();
    let (tasks, ledger, owner) = fixture(&dir.path().join("profiles"));
    ledger.close_fence();
    let (o, b) = request(1, 1);
    assert_eq!(owner.open(o, b).await, Err(OwnerError::Stopping));
    assert_eq!(ledger.child_stats().registered, 0);
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}

#[tokio::test]
async fn abrupt_core_exit_closes_helper_and_next_core_collects_confirmed_profile() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("evorch-renderer-test-helper")
        });
    support::core_eof(exe, &dir.path().join("profiles")).await;
}

#[tokio::test]
async fn partial_reader_and_ipc_worker_failure_retains_group_until_reclaimed() {
    for fail in [0, 1, 2] {
        let mut tree = support::Tree::new();
        let dir = tempfile::tempdir().unwrap();
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let ledger = SpawnLedger::new();
        let exe = std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("evorch-renderer-test-helper")
            });
        let mut config = OwnerConfig::new(exe, dir.path().join("profiles"));
        config.fail_after_readers = Some(fail);
        let (spawned, rx) = std::sync::mpsc::channel();
        config.spawned = Some(spawned);
        let owner = RendererOwner::new(&tasks, ledger.clone(), config).unwrap();
        let (o, b) = request(1, 1);
        assert!(owner.open(o, b).await.is_err());
        assert!(ledger.children().is_empty());
        assert_eq!(ledger.child_stats().reaped, 1);
        tree.track_pid(rx.recv().unwrap());
        shutdown(tasks, &ledger).await;
        tree.assert_clean();
    }
}

#[tokio::test(start_paused = true)]
async fn crashed_generation_is_reaped_and_restarts_only_after_backoff() {
    let profiles = tempfile::tempdir().unwrap();
    let mut tree = support::Tree::new();
    let (tasks, ledger, owner) = fixture(&profiles.path().join("profiles"));
    let old = {
        let (o, b) = request(64, 48);
        owner.open(o, b)
    }
    .await
    .unwrap();
    tree.track(&ledger);
    ledger.stop_all_children();
    let mut status = owner.subscribe();
    while status.borrow().phase != Phase::Failed {
        status.changed().await.unwrap();
    }
    assert_eq!(status.borrow().tabs, 0);
    for _ in 0..=renderer_ipc::MAX_TABS {
        assert_eq!(
            {
                let (o, b) = request(64, 48);
                owner.open(o, b)
            }
            .await,
            Err(OwnerError::Busy)
        );
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let new = {
        let (o, b) = request(64, 48);
        owner.open(o, b)
    }
    .await
    .unwrap();
    tree.track(&ledger);
    assert!(new.generation > old.generation);
    let mut additional = Vec::new();
    for _ in 1..renderer_ipc::MAX_TABS {
        let (o, b) = request(64, 48);
        additional.push(owner.open(o, b).await.unwrap());
    }
    assert_eq!(owner.status().tabs, renderer_ipc::MAX_TABS);
    for tab in additional {
        owner.close(tab).await.unwrap();
    }
    assert_eq!(owner.close(old).await, Err(OwnerError::StaleGeneration));
    owner.set_streaming(new, false).await.unwrap();
    owner.set_frame_rate(new, 10).await.unwrap();
    owner.set_streaming(new, true).await.unwrap();
    owner.close(new).await.unwrap();
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}

#[tokio::test]
async fn cancelling_open_does_not_strand_a_tab_or_its_reservation() {
    let mut tree = support::Tree::new();
    let dir = test_socket_dir::SocketDir::new();
    let root = dir.path().join("p");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::fs::write(root.join("fixture-mode"), "gate-hello").unwrap();
    let listener = tokio::net::UnixListener::bind(root.join("gate.sock")).unwrap();
    let (tasks, ledger, owner) = fixture(&root);
    let opening = {
        let owner = owner.clone();
        tokio::spawn(async move {
            let (o, b) = request(1, 1);
            owner.open(o, b).await
        })
    };
    let (mut gate, _) = listener.accept().await.unwrap();
    let mut ready = [0];
    gate.read_exact(&mut ready).await.unwrap();
    tree.track(&ledger);
    opening.abort();
    assert!(opening.await.unwrap_err().is_cancelled());
    gate.write_all(b"G").await.unwrap();
    let mut status = owner.subscribe();
    while status.borrow().phase != Phase::Idle {
        status.changed().await.unwrap();
    }
    assert_eq!(status.borrow().tabs, 0);
    let (o, b) = request(3840, 2160);
    let tab = owner.open(o, b).await.unwrap();
    owner.close(tab).await.unwrap();
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}
#[tokio::test]
async fn open_artifact_tags_the_run_tab_with_its_record_and_plain_open_does_not() {
    use runtime::artifacts::{ArtifactPages as _, ArtifactRecord};
    use workbench_host::areas::browser::run_pages::RuntimePages;
    let mut tree = support::Tree::new();
    let dir = tempfile::tempdir().unwrap();
    let (tasks, ledger, owner) = fixture(&dir.path().join("profiles"));
    let pages = RuntimePages::new(Some(owner));
    let record = ArtifactRecord {
        artifact_id: "artifact-1".into(),
        sha256: "0".repeat(64),
        media_type: "text/html".into(),
        extension: "html".into(),
        title: "page".into(),
        caption: None,
        source_path: "/unused".into(),
        byte_len: 0,
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        thread_id: Some("thread-1".into()),
        inlined_assets: Vec::new(),
    };
    pages
        .open_artifact("run-1", b"<p>tagged</p>".to_vec(), &record)
        .await
        .unwrap();
    tree.track(&ledger);
    assert_eq!(pages.artifact_of("run-1"), Some(record));
    // The default path keeps the previous behaviour: a page without a record.
    pages.open("run-2", b"<p>plain</p>".to_vec()).await.unwrap();
    assert!(pages.page("run-2").is_some());
    assert_eq!(pages.artifact_of("run-2"), None);
    assert!(pages.close("run-1") && pages.close("run-2"));
    shutdown(tasks, &ledger).await;
    tree.assert_clean();
}
