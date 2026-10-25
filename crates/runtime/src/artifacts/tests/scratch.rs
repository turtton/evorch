//! `render_artifact` in a sandboxed runtime reads the thread's `$TMPDIR` (ADR 0038).
use std::os::unix::fs::DirBuilderExt as _;

use base64::Engine as _;

use super::inspect::{Pages, open_last_capture};
use super::*;
use crate::artifacts::ArtifactPages;
use crate::scratch::ThreadScratch;

fn private_dir(path: &Path) {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
}

fn read(path: &Path) -> Step {
    calls(vec![("read", json!({"path": path}))])
}

// Given: a visual Worker whose thread's $TMPDIR holds an HTML mock and the image it
//        references, another thread's $TMPDIR, and an allowed directory
// When: it captures the mock, opens it, and tries files of the other two
// Then: the mock is stored with its image embedded and opens with no external
//       reference; the other thread's file and the allowed directory's file are
//       refused, although `read` reaches the allowed directory
#[tokio::test]
async fn visual_worker_captures_a_mock_from_its_threads_tmpdir() {
    let base = tempfile::tempdir().unwrap();
    let scratch = Arc::new(ThreadScratch::at(base.path().join("scratch")));
    let tmpdir = scratch.thread_dir("thread");
    let other = scratch.thread_dir("other");
    private_dir(&tmpdir.join("img"));
    private_dir(&other);
    let hero = tmpdir.join("img/hero.png");
    std::fs::write(&hero, PNG).unwrap();
    let mock = tmpdir.join("mock.html");
    let html = |image: &str, background: &str| {
        format!(r#"<!doctype html><img src="{image}"><div style="background:{background}"></div>"#)
    };
    std::fs::write(&mock, html("img/hero.png", "url('img/hero.png')")).unwrap();
    let foreign = other.join("foreign.png");
    std::fs::write(&foreign, PNG).unwrap();
    let allowed = tempfile::tempdir().unwrap();
    let shared = allowed.path().join("shared.png");
    std::fs::write(&shared, PNG).unwrap();
    std::fs::write(allowed.path().join("notes.txt"), "allowed notes").unwrap();

    let harness = Harness::sandboxed(Arc::clone(&scratch));
    let pages = Arc::new(Pages::default());
    harness
        .runtime
        .set_artifact_pages(Arc::clone(&pages) as Arc<dyn ArtifactPages>);
    harness.runtime.set_project_allowed_directories(
        harness.workspace.path().to_path_buf(),
        vec![allowed.path().to_path_buf()],
    );
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let root = harness.root(
        "thread",
        Role::Orchestrator,
        vec![
            Step::HoldThen(started.clone(), release.clone(), Box::new(|_| vec![])),
            stop(),
        ],
    );
    started.notified().await;
    let child = harness.visual_child(
        root,
        vec![
            render(mock.to_str().unwrap(), "Hero mock"),
            open_last_capture(),
            render(foreign.to_str().unwrap(), "Other thread"),
            read(&allowed.path().join("notes.txt")),
            render(shared.to_str().unwrap(), "Allowed directory"),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    release.notify_one();
    harness.runtime.wait(root).await.unwrap();

    let results = harness.results(child);
    assert!(!results[0].0 && !results[1].0, "{results:?}");
    let captured = serde_json::from_str::<Value>(&results[0].1).unwrap();
    assert_eq!(captured["inlined_images"], 1, "{captured}");
    let record = harness
        .runtime
        .artifact_store()
        .unwrap()
        .load(captured["artifact_id"].as_str().unwrap())
        .unwrap();
    let canonical = |path: &Path| path.canonicalize().unwrap().to_string_lossy().into_owned();
    assert_eq!(record.source_path, canonical(&mock));
    assert_eq!(record.inlined_assets.len(), 1);
    assert_eq!(record.inlined_assets[0].source_path, canonical(&hero));
    let data = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(PNG)
    );
    let embedded = html(&data, &format!("url({data})"));
    assert_eq!(
        pages.opened.lock().unwrap()[0],
        (child.to_string(), embedded.into_bytes()),
        "the page gets the mock with nothing left to load"
    );
    let outside = "is outside this run's workspace and its $TMPDIR";
    assert!(
        results[2].0 && results[2].1.contains(outside),
        "{results:?}"
    );
    assert!(
        !results[3].0 && results[3].1.contains("allowed notes"),
        "{results:?}"
    );
    assert!(
        results[4].0 && results[4].1.contains(outside),
        "{results:?}"
    );
}
