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

const NOT_ATTACHED: &str = "Not attached to an active page";

fn refuse(connection: &Arc<Connection>, start: &Value, message: &str) {
    connection.deliver(json!({"id":start["id"],"error":{"code":-32000,"message":message}}));
}

fn jpeg_frame() -> Value {
    let mut jpeg = Vec::new();
    image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]))
        .write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
    let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, jpeg);
    json!({"sessionId":"s1","method":"Page.screencastFrame","params":{"data":data,"sessionId":1}})
}

fn drained(commands: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut methods = Vec::new();
    while let Ok(message) = commands.try_recv() {
        let value: Value = serde_json::from_str(&message).unwrap();
        methods.push(value["method"].as_str().unwrap_or_default().to_owned());
    }
    methods
}

#[tokio::test(start_paused = true)]
async fn start_refused_while_the_page_navigates_is_retried_with_coherent_metadata() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let mut updates = feed.updates.clone();
    let refused = command(&connection, &mut commands, "Page.startScreencast").await;
    refuse(&connection, &refused, NOT_ATTACHED);
    // The navigation finishes while the producer waits to retry.
    connection.deliver(json!({"sessionId":"s1","method":"Page.loadEventFired","params":{}}));
    let info = command(&connection, &mut commands, "Target.getTargetInfo").await;
    connection.deliver(json!({"id":info["id"],"result":{"targetInfo":{"targetId":"target1","title":"navigated","url":"https://native.invalid/"}}}));
    let retry = command(&connection, &mut commands, "Page.startScreencast").await;
    assert_ne!(retry["id"], refused["id"]);
    {
        let page = updates.borrow_and_update();
        let page = page.as_ref().expect("the observation did not end");
        assert_eq!(page.tabs[0].title, "navigated");
        assert!(page.frame.is_none(), "no pixels without a live screencast");
    }
    connection.deliver(json!({"id":retry["id"],"result":{}}));
    connection.deliver(jpeg_frame());
    let page = updates
        .wait_for(|page| page.as_ref().is_some_and(|page| page.frame.is_some()))
        .await
        .unwrap()
        .clone()
        .unwrap();
    assert_eq!(
        page.frame.as_ref().unwrap().tab,
        *page.active_tab.as_ref().unwrap()
    );
    assert_eq!(page.tabs[0].title, "navigated");
    drop(updates);
    tokio::join!(feed.close(), async {
        let stop = command(&connection, &mut commands, "Page.stopScreencast").await;
        connection.deliver(json!({"id":stop["id"],"result":{}}));
    });
}

#[tokio::test(start_paused = true)]
async fn a_start_that_is_always_refused_is_retried_a_bounded_number_of_times_then_ends() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let mut updates = feed.updates.clone();
    let mut attempts = 0;
    loop {
        tokio::select! {
            ended = updates.wait_for(Option::is_none) => {
                ended.unwrap();
                break;
            }
            start = command(&connection, &mut commands, "Page.startScreencast") => {
                attempts += 1;
                refuse(&connection, &start, NOT_ATTACHED);
            }
        }
    }
    assert_eq!(attempts, START_RETRY_DELAYS.len() + 1);
    let sent = drained(&mut commands);
    assert!(
        !sent
            .iter()
            .any(|m| m == "Page.startScreencast" || m == "Page.stopScreencast"),
        "{sent:?}"
    );
    feed.close().await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_is_not_retried() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let mut updates = feed.updates.clone();
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    refuse(
        &connection,
        &start,
        crate::agent::connection::CONNECTION_CLOSED,
    );
    updates.wait_for(Option::is_none).await.unwrap();
    assert!(
        !drained(&mut commands)
            .iter()
            .any(|m| m == "Page.startScreencast")
    );
    feed.close().await;
}

#[tokio::test(start_paused = true)]
async fn closing_while_waiting_to_retry_does_not_start_again() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    refuse(&connection, &start, NOT_ATTACHED);
    feed.close().await;
    assert!(
        !drained(&mut commands)
            .iter()
            .any(|m| m == "Page.startScreencast")
    );
}

#[tokio::test(start_paused = true)]
async fn a_page_that_closes_while_waiting_to_retry_ends_the_observation() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let mut updates = feed.updates.clone();
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    refuse(&connection, &start, NOT_ATTACHED);
    browser.release("run");
    updates.wait_for(Option::is_none).await.unwrap();
    assert!(
        !drained(&mut commands)
            .iter()
            .any(|m| m == "Page.startScreencast")
    );
    feed.close().await;
}

#[tokio::test(start_paused = true)]
async fn switching_tabs_while_waiting_to_retry_starts_the_new_tab_at_once() {
    let (browser, connection, mut commands) = fixture();
    let execution = browser.observation_pages().pop().unwrap();
    let feed = browser.observe(&execution).unwrap();
    let start = command(&connection, &mut commands, "Page.startScreencast").await;
    assert_eq!(start["sessionId"], "s1");
    refuse(&connection, &start, NOT_ATTACHED);
    browser.shared.attached(&connection, &json!({"sessionId":"s2","targetInfo":{"targetId":"popup","openerId":"target1","url":"about:blank"}}));
    browser.switch_tab("run", "t2").await.unwrap();
    let next = command(&connection, &mut commands, "Page.startScreencast").await;
    assert_eq!(next["sessionId"], "s2");
    drop(feed);
}
