//! The core-client page against a production core with the real renderer helper (real CEF):
//! a conversation reaches an artifact's body, its preview (frames as a texture) and the
//! preview's inspection result by clicks, and a window that exits cleanly or is killed lets
//! go of its own tab only (Stage 16b PR V). Every wait observes an explicit change of the
//! shared client or of the core. The tests are `#[ignore]`d: the `cef-e2e` job supplies
//! `EVORCH_RENDERER_EXE`.
#![cfg(target_os = "linux")]
use egui_kittest::kittest::Queryable;
use gui::core_client::connection::Action;
use workbench_protocol::{
    namespaces::browser::{BrowserQuery, BrowserReply},
    queries::{Query, QueryReply},
};

#[allow(dead_code)]
#[path = "../../evorch-core/tests/renderer_support/data_peer.rs"]
mod data_peer;
#[path = "cef_support/mod.rs"]
mod support;
use support::{Window, open_window};

type NoRenderer = egui_kittest::LazyRenderer;

/// Activates the widget by keyboard (focus it, then Enter), not by a pointer at the place where
/// it was last drawn. The page is still laying out while the core's answers arrive, so a widget
/// can move between the frame the test saw it in and the frame that handles the pointer; egui
/// then does not count the click, and the test would wait for a result that nobody asked for.
/// A focused widget is found by its accessibility id, wherever it has moved to.
fn press_enter_on_focus(window: &mut Window) {
    window.harness.run_steps(1);
    window.harness.key_press(egui::Key::Enter);
    window.harness.run_steps(8);
}

fn click(window: &mut Window, label: &str) {
    window
        .harness
        .get_all_by_label_contains(label)
        .next()
        .unwrap_or_else(|| panic!("{label:?} is not on the page"))
        .focus();
    press_enter_on_focus(window);
}

fn click_exact(window: &mut Window, label: &str) {
    window.harness.get_by_label(label).focus();
    press_enter_on_focus(window);
}

fn shows_exact(window: &Window, label: &str) -> bool {
    window.harness.query_by_label(label).is_some()
}

fn shows(window: &Window, label: &str) -> bool {
    window.harness.query_by_label_contains(label).is_some()
}

fn frames_taken(window: &Window) -> u64 {
    window
        .shared
        .lock()
        .unwrap()
        .features
        .data_stats()
        .frames_taken
}

async fn tabs_used(peer: &data_peer::Peer) -> u32 {
    match peer
        .client
        .query(Query::Browser(BrowserQuery::GetRendererStatus {}))
        .await
        .unwrap()
    {
        QueryReply::Browser(BrowserReply::Status(status)) => status.tabs_used,
        other => panic!("unexpected reply: {other:?}"),
    }
}

/// Waits for the shared stream of events until the helper holds `expected` tabs.
async fn until_tabs(peer: &mut data_peer::Peer, expected: u32) {
    while tabs_used(peer).await != expected {
        peer.events.next().await.expect("the stream stays open");
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires actual CEF; the cef-e2e job supplies EVORCH_RENDERER_EXE"]
async fn a_conversation_reaches_the_artifact_its_preview_and_its_inspection() {
    support::use_real_helper();
    let env = support::start(&[(
        "page.html",
        support::STAMP_PAGE.as_bytes().to_vec(),
        "text/html",
    )])
    .await;
    let mut window = open_window(&env, egui::vec2(1100.0, 1500.0), None::<NoRenderer>);

    // From the conversation: its "Artifacts" header button brings the artifacts tab
    // forward, where the thread's artifact is listed.
    window.until(|w| shows_exact(w, "Artifacts")).await;
    click_exact(&mut window, "Artifacts");
    window.until(|w| shows(w, "page.html  (")).await;
    click(&mut window, "page.html  (");
    // The choice is taken within the frame of the click: the artifact is opened under its
    // title (an exact label; the row's label is longer). A click that did not land fails here
    // by itself, not as a wait for a body that was never asked for.
    assert!(
        shows_exact(&window, "page.html"),
        "the click on the artifact row was not taken"
    );
    window.until(|w| shows_exact(w, "Open preview")).await;
    window.until(|w| shows(w, "<canvas")).await;

    // Its preview: the browser section opens by itself and frames reach the texture.
    click_exact(&mut window, "Open preview");
    window.until(|w| frames_taken(w) > 0).await;
    window.until(|w| !shows(w, "No preview is open")).await;
    let stats = window.shared.lock().unwrap().features.data_stats();
    assert!(stats.frames_received >= stats.frames_taken && stats.bytes_held < 40 << 20);

    // Its inspection result.
    click_exact(&mut window, "Inspect");
    click_exact(&mut window, "DOM");
    window.until(|w| shows(w, "Inspection: DOM")).await;
    assert_eq!(
        env.run_tabs(),
        0,
        "the preview is the client's, not a run's"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires actual CEF; the cef-e2e job supplies EVORCH_RENDERER_EXE"]
async fn a_window_that_exits_or_is_killed_releases_only_its_own_tab() {
    support::use_real_helper();
    let page = support::STAMP_PAGE.as_bytes().to_vec();
    let env = support::start(&[("page.html", page.clone(), "text/html")]).await;
    env.run_opens(0, &page).await;
    assert_eq!(env.run_tabs(), 1);
    let left = std::sync::Arc::new(tokio::sync::Notify::new());
    {
        let left = left.clone();
        env.boot
            .host
            .lock()
            .unwrap()
            .connections()
            .register(std::sync::Arc::new(move |_, _| left.notify_one()));
    }
    let mut watcher = data_peer::Peer::connect(&env.socket, "watcher").await;

    let preview = |env: &support::Env| {
        let window = open_window(env, egui::vec2(1100.0, 1500.0), None::<NoRenderer>);
        window
            .shared
            .lock()
            .unwrap()
            .features
            .open_preview(env.artifacts[0].0.clone());
        window
    };
    let (mut exiting, mut killed, mut bystander) = (preview(&env), preview(&env), preview(&env));
    for window in [&mut exiting, &mut killed, &mut bystander] {
        window.until(|w| frames_taken(w) > 0).await;
    }
    until_tabs(&mut watcher, 4).await;

    // A clean exit closes its tab: the one this window opened, and nothing else.
    exiting.actions.send(Action::Shutdown).await.unwrap();
    (&mut exiting.actor).await.unwrap();
    until_tabs(&mut watcher, 3).await;
    assert_eq!(env.run_tabs(), 1, "the run's tab stays");

    // A kill sends nothing: the core keeps the tab for its grace and everything else runs.
    killed.actor.abort();
    let _ = (&mut killed.actor).await;
    drop(killed);
    left.notified().await;
    assert_eq!(tabs_used(&watcher).await, 3, "no command closed the tab");
    assert_eq!(env.run_tabs(), 1);
    let before = frames_taken(&bystander);
    bystander.until(|w| frames_taken(w) > before + 2).await;
}
