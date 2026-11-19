use super::*;
use futures_util::poll;
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
fn fixture() -> (
    AgentBrowser,
    Arc<Connection>,
    mpsc::UnboundedReceiver<String>,
) {
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Allow),
        max_pages: 4,
        max_tabs: 4,
    });
    let (connection, commands) = Connection::test_pair();
    *lock(&browser.shared.connection) = Some(connection.clone());
    *lock(&browser.shared.creation) = Some(Arc::new(Creation {
        stamp: Some(Arc::new(super::super::super::observation::ExecutionStamp {
            run: "run".into(),
            incarnation: "one".into(),
        })),
        browser_generation: 1,
        page_generation: 1,
    }));
    browser.shared.edit(|r| r.creating(1));
    browser.shared.attached(&connection, &json!({"sessionId":"s1", "targetInfo":{"targetId":"target1","title":"first","url":"about:blank"}}));
    browser.shared.edit(|r| {
        r.claim("run", "target1");
        r.creating(-1);
    });
    lock(&browser.shared.creation).take();
    (browser, connection, commands)
}
async fn command(
    connection: &Arc<Connection>,
    commands: &mut mpsc::UnboundedReceiver<String>,
    method: &str,
) -> Value {
    loop {
        let value: Value = serde_json::from_str(&commands.recv().await.unwrap()).unwrap();
        if value["method"] == method {
            return value;
        }
        connection.deliver(json!({"id":value["id"],"result":{}}));
    }
}
#[tokio::test]
async fn one_producer_fanout_and_replacement_wait_for_old_stop() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let first = browser.observe(&execution).unwrap();
    let second = browser.observe(&execution).unwrap();
    assert!(Arc::ptr_eq(&first.inner, &second.inner));
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    connection.deliver(json!({"id":start["id"],"result":{}}));
    first.close().await;
    let closing = second.close();
    tokio::pin!(closing);
    assert!(poll!(&mut closing).is_pending());
    let stop = command(&connection, &mut commands, "Page.stopScreencast").await;
    let (_, tab) = browser.shared.active("run").unwrap();
    assert!(
        tab.streaming.try_lock().is_err(),
        "stop still owns its lease"
    );
    let replacement = browser.observe(&execution).unwrap();
    assert!(tab.streaming.try_lock().is_err());
    // A direct competing producer cannot start while the old stop is held.
    let (tx, _rx) = watch::channel(None);
    {
        let competing = browser.shared.produce("run", tab.clone(), &tx);
        tokio::pin!(competing);
        assert!(poll!(&mut competing).is_pending());
    }
    connection.deliver(json!({"id":stop["id"],"result":{}}));
    closing.await;
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    connection.deliver(json!({"id":start["id"],"result":{}}));
    let closing = replacement.close();
    tokio::pin!(closing);
    assert!(poll!(&mut closing).is_pending());
    let stop = command(&connection, &mut commands, "Page.stopScreencast").await;
    connection.deliver(json!({"id":stop["id"],"result":{}}));
    closing.await;
    assert!(
        browser.open_page("run").await.is_some(),
        "closing observation never closes the page"
    );
}
#[tokio::test]
async fn tab_switch_drops_old_pixels_and_old_execution_cannot_be_relabelled() {
    let (browser, connection, _commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let original = browser.observation_snapshot(&execution).unwrap();
    let old_frame = Arc::new(Frame {
        tab: original.active_tab.clone().unwrap(),
        sequence: 1,
        width: 1,
        height: 1,
        rgba: Arc::from([0, 0, 0, 255]),
    });
    browser.shared.attached(&connection, &json!({"sessionId":"s2","targetInfo":{"targetId":"popup","openerId":"target1","url":"about:blank"}}));
    browser.switch_tab("run", "t2").await.unwrap();
    let switched = browser
        .shared
        .publication(
            "run",
            &browser.shared.active("run").unwrap().1.creation,
            Some(old_frame),
            2,
        )
        .unwrap();
    assert_ne!(switched.active_tab, original.active_tab);
    assert!(switched.frame.is_none());
    assert_eq!(switched.execution.as_ref(), Some(&execution));
    let mut forged = execution.clone();
    forged.stamp = Arc::new(super::super::super::observation::ExecutionStamp {
        run: "run".into(),
        incarnation: "two".into(),
    });
    assert!(!browser.observation_current(&forged));
    assert!(browser.observe(&forged).is_err());
    assert!(browser.observation_snapshot(&forged).is_none());
}

#[tokio::test]
async fn load_refreshes_title_before_the_ordered_session_reply() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    connection.deliver(json!({"sessionId":"s1","method":"Page.loadEventFired","params":{}}));
    let barrier = connection.call(Some("s1"), "Runtime.evaluate", json!({}));
    let barrier_command = command(&connection, &mut commands, "Runtime.evaluate").await;
    connection.deliver(json!({"id":barrier_command["id"],"result":{}}));
    let info = command(&connection, &mut commands, "Target.getTargetInfo").await;
    connection.deliver(json!({"id":info["id"],"result":{"targetInfo":{"targetId":"target1","title":"loaded title","url":"https://native.invalid/"}}}));
    barrier.await.unwrap();
    let page = browser.observation_snapshot(&execution).unwrap();
    assert_eq!(page.tabs[0].title, "loaded title");
    assert_eq!(page.tabs[0].url, "https://native.invalid/");
}

#[tokio::test]
async fn browser_restart_rejects_aba_and_late_old_connection_events() {
    let (browser, old_connection, _old_commands) = fixture();
    let old = browser.observation_pages().pop().unwrap();
    let old_feed = browser.observe(&old).unwrap();
    let creation = browser.shared.active("run").unwrap().1.creation.clone();
    browser.shared.reset();
    assert!(!browser.observation_current(&old));
    let (connection, _commands) = Connection::test_pair();
    *lock(&browser.shared.connection) = Some(connection.clone());
    *lock(&browser.shared.creation) = Some(Arc::new(Creation {
        stamp: Some(old.stamp.clone()),
        browser_generation: browser.shared.generation.load(Ordering::Relaxed),
        page_generation: old.page_generation,
    }));
    browser.shared.edit(|r| r.creating(1));
    browser.shared.attached(&connection, &json!({"sessionId":"s1","targetInfo":{"targetId":"target1","title":"replacement","url":"about:blank"}}));
    browser.shared.edit(|r| {
        r.claim("run", "target1");
        r.creating(-1);
    });
    lock(&browser.shared.creation).take();
    let new = browser.observation_pages().pop().unwrap();
    assert_ne!(old.browser_generation, new.browser_generation);
    assert!(!browser.observation_current(&old));
    browser.shared.on_browser_event(&old_connection, None);
    browser.shared.on_browser_event(
        &old_connection,
        Some(Event {
            method: "Target.detachedFromTarget".into(),
            params: json!({"sessionId":"s1","targetId":"target1"}),
        }),
    );
    assert!(browser.observation_current(&new));
    assert!(browser.observe(&old).is_err());
    assert!(browser.shared.feed("run", Some(&old)).is_err());
    assert!(
        browser
            .shared
            .publication("run", &creation, None, 9)
            .is_none()
    );
    let new_feed = browser.observe(&new).unwrap();
    assert!(!Arc::ptr_eq(&old_feed.inner, &new_feed.inner));
    assert_eq!(
        new_feed
            .updates
            .borrow()
            .as_ref()
            .unwrap()
            .execution
            .as_ref(),
        Some(&new)
    );
    tokio::join!(old_feed.close(), new_feed.close());
}
