//! Real CEF through the production core owner, with test-only subreaper checks.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]
#[path = "renderer_support/mod.rs"]
mod support;
use renderer_ipc::{OpenArtifact, SessionId, TabId, Viewport};
use workbench_host::areas::{
    AreaTasks,
    browser::renderer::{OwnerConfig, RendererOwner},
};
fn executable() -> std::path::PathBuf {
    let exe = std::env::var_os("EVORCH_RENDERER_EXE")
        .map(std::path::PathBuf::from)
        .expect("cef-e2e must supply EVORCH_RENDERER_EXE");
    assert!(exe.is_absolute() && exe.is_file());
    assert!(exe.with_file_name("evorch-cef-helper").is_file());
    exe
}
#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn real_owner_reclaims_graceful_and_forced_groups_without_touching_unrelated_child() {
    // Baseline child stays alive across both renderer generations.
    let mut unrelated = tokio::process::Command::new("sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut tree = support::Tree::new();
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger.clone(),
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    for graceful in [true, false] {
        let (open, body) = OpenArtifact::with_files(
            SessionId(1),
            TabId(1),
            Viewport {
                width: 1024,
                height: 768,
            },
            30,
            30_000,
            vec![(
                "index.html".into(),
                "text/html".into(),
                b"<canvas>renderer owner</canvas>".to_vec(),
            )],
        );
        let tab = owner.open(open, body).await.unwrap();
        tree.track(&ledger);
        owner.set_frame_rate(tab, 10).await.unwrap();
        owner.set_streaming(tab, false).await.unwrap();
        owner.set_streaming(tab, true).await.unwrap();
        if graceful {
            owner.close(tab).await.unwrap();
            owner.stop().await.unwrap();
        } else {
            ledger.stop_all_children();
            let mut view = owner.subscribe();
            view.wait_for(|s| s.phase == workbench_host::areas::browser::renderer::Phase::Failed)
                .await
                .unwrap();
        }
        assert!(ledger.children().is_empty());
        assert!(unrelated.try_wait().unwrap().is_none());
    }
    ledger.close_fence();
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    ledger.reap_all_children().await.unwrap();
    ledger.join_workers().await.unwrap();
    tree.assert_clean();
    drop(unrelated.stdin.take());
    unrelated.wait().await.unwrap();
    assert_eq!(
        std::fs::read_dir(root.path().join("profiles"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn abrupt_real_core_exit_ends_helper_by_eof_and_collects_the_old_profile() {
    let dir = tempfile::tempdir().unwrap();
    support::core_eof(executable(), &dir.path().join("profiles")).await;
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn inspect_artifact_pages_open_through_the_core_owner_with_the_owned_record() {
    use runtime::artifacts::{ArtifactPages as _, ArtifactRecord};
    use workbench_host::areas::browser::run_pages::RuntimePages;
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger,
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    let pages = RuntimePages::new(Some(owner.clone()));
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
    let page = pages
        .open_artifact("run-1", b"<button>inspect me</button>".to_vec(), &record)
        .await
        .unwrap();
    assert!(pages.page("run-1").is_some());
    let png = page.agent().screenshot().await.unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    // IHDR: the screenshot is the 1024x768 viewport.
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1024);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 768);
    assert!(pages.close("run-1"));
    owner.stop().await.unwrap();
}
