//! Explicit real-Chromium coverage of the read-only source.
use browser::agent::{
    Activity, AgentBrowser, LaunchOptions, PageProvider, RequestPolicy,
    observation::{
        ExecutionStamp,
        source::{Feed, Publication},
        with_execution_stamp,
    },
};
use serde_json::json;
use std::sync::Arc;
struct Allow;
#[async_trait::async_trait]
impl RequestPolicy for Allow {
    async fn resolve(&self, _: &str, _: u16, _: bool) -> Result<Vec<std::net::SocketAddr>, String> {
        Err("external networking disabled in this fixture".into())
    }
    async fn check(&self, _: &url::Url) -> Result<(), String> {
        Ok(())
    }
}
async fn until(feed: &mut Feed, predicate: impl Fn(&Publication) -> bool) -> Arc<Publication> {
    loop {
        if let Some(page) = feed
            .updates
            .borrow_and_update()
            .as_ref()
            .filter(|page| predicate(page))
            .cloned()
        {
            return page;
        }
        feed.updates.changed().await.expect("source still live");
    }
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn coherent_shared_source_follows_popup_activity_and_closure_without_restamping() {
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Allow),
        max_pages: 4,
        max_tabs: 4,
    });
    assert!(browser.observation_pages().is_empty());
    let stamp = Arc::new(ExecutionStamp {
        run: "run".into(),
        incarnation: "first".into(),
    });
    let page = with_execution_stamp(stamp.clone(), browser.page("run"))
        .await
        .unwrap();
    let execution = browser.observation_pages().pop().unwrap();
    assert!(Arc::ptr_eq(&execution.stamp, &stamp));
    let mut first = browser.observe(&execution).unwrap();
    let mut second = browser.observe(&execution).unwrap();
    page.navigate("data:text/html,<title>Source</title><body>original</body>")
        .await
        .unwrap();
    let original = until(&mut first, |p| {
        p.frame.is_some() && p.tabs.iter().any(|t| t.title == "Source")
    })
    .await;
    let other = until(&mut second, |p| {
        p.revision >= original.revision && p.frame.is_some()
    })
    .await;
    assert_eq!(
        original.frame.as_ref().unwrap().tab,
        other.frame.as_ref().unwrap().tab
    );
    let newer = Arc::new(ExecutionStamp {
        run: "run".into(),
        incarnation: "second".into(),
    });
    with_execution_stamp(newer.clone(), browser.page("run"))
        .await
        .unwrap();
    assert_eq!(
        browser.observation_pages().as_slice(),
        std::slice::from_ref(&execution),
        "never stamp an existing page"
    );
    page.target()
        .call(
            "Runtime.evaluate",
            json!({"expression":"window.open('about:blank')"}),
        )
        .await
        .unwrap();
    until(&mut second, |p| p.tabs.len() == 2).await;
    let popup = browser.switch_tab("run", "t2").await.unwrap();
    let switched = until(&mut second, |p| p.active_tab != original.active_tab).await;
    assert_eq!(
        switched.execution.as_ref(),
        Some(&execution),
        "popup inherits opener stamp"
    );
    if let Some(frame) = &switched.frame {
        assert_eq!(Some(&frame.tab), switched.active_tab.as_ref());
    }
    first.close().await;
    popup.target().call("Runtime.evaluate", json!({"expression":"document.documentElement.style.background='rgb(0,0,255)'; alert('source activity')"})).await.unwrap();
    let blue = until(&mut second, |p| {
        p.frame.as_ref().is_some_and(|frame| {
            let i = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
            frame.rgba[i] < 40 && frame.rgba[i + 1] < 40 && frame.rgba[i + 2] > 200
        }) && p.activity.iter().any(|a| matches!(a, Activity::Dialog(_)))
    })
    .await;
    assert_eq!(
        Some(&blue.frame.as_ref().unwrap().tab),
        blue.active_tab.as_ref()
    );
    assert!(blue.frame.as_ref().unwrap().rgba.len() <= 4_096_000);
    assert!(
        !popup.take_dialogs().is_empty(),
        "observation must not consume agent inputs"
    );
    second.close().await;
    assert!(browser.open_page("run").await.is_some());
    let mut restarted = browser.observe(&execution).unwrap();
    until(&mut restarted, |p| p.frame.is_some()).await;
    browser.close_tab("run", "t2").await.unwrap();
    until(&mut restarted, |p| {
        p.tabs.len() == 1 && p.active_tab == original.active_tab
    })
    .await;
    assert!(browser.close("run").await);
    restarted.updates.wait_for(|p| p.is_none()).await.unwrap();
    restarted.close().await;
    assert!(!browser.observation_current(&execution));
    with_execution_stamp(newer, browser.page("run"))
        .await
        .unwrap();
    let replacement = browser.observation_pages().pop().unwrap();
    assert_ne!(replacement.page_generation, execution.page_generation);
    assert_eq!(replacement.stamp.incarnation, "second");
    assert!(browser.observe(&execution).is_err());
    browser.close("run").await;
}

#[test]
fn source_queries_do_not_launch_a_browser_or_invent_a_stamp() {
    let browser = AgentBrowser::new(LaunchOptions {
        executable: Some("/nonexistent/observation-must-not-launch".into()),
        policy: Arc::new(Allow),
        max_pages: 2,
        max_tabs: 2,
    });
    let execution = browser::agent::observation::source::ExecutionPage {
        stamp: Arc::new(ExecutionStamp {
            run: "missing".into(),
            incarnation: "unknown".into(),
        }),
        browser_generation: 1,
        page_generation: 1,
    };
    assert!(browser.observation_pages().is_empty());
    assert!(!browser.observation_current(&execution));
    assert!(browser.observation_snapshot(&execution).is_none());
    assert!(browser.observe(&execution).is_err());
    assert!(browser.pages().borrow().is_empty());
}
