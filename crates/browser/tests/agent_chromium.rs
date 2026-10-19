use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use browser::agent::{
    Activity, AgentBrowser, AgentError, DialogAnswer, LaunchOptions, PageAgent, PageProvider,
    RequestPolicy, WaitFor, WaitLimits, WaitOutcome, Waited,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PAGE: &str = r#"<!doctype html><html><head><title>Agent fixture</title></head><body>
<h1>Agent fixture</h1>
<img src="http://blocked.invalid/pixel.png" alt="">
<label>Name <input id="name"></label>
<button onclick="document.getElementById('out').textContent = 'Hello ' + document.getElementById('name').value">Greet</button>
<p id="out">waiting</p>
</body></html>"#;

/// Refuses one host and one port, and resolves everything else with the
/// system resolver, as the proxy requires.
struct Refuse {
    host: &'static str,
    port: u16,
}

impl Refuse {
    fn verdict(&self, host: &str, port: u16) -> Result<(), String> {
        if host == self.host || port == self.port {
            Err("blocked by test policy".into())
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl RequestPolicy for Refuse {
    async fn check(&self, url: &url::Url) -> Result<(), String> {
        self.verdict(
            url.host_str().unwrap_or_default(),
            url.port_or_known_default().unwrap_or_default(),
        )
    }

    async fn resolve(&self, host: &str, port: u16, _tls: bool) -> Result<Vec<SocketAddr>, String> {
        self.verdict(host, port)?;
        tokio::net::lookup_host((host, port))
            .await
            .map(Iterator::collect)
            .map_err(|error| error.to_string())
    }
}

async fn serve(page: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let page = page.clone();
            tokio::spawn(async move {
                let mut request = [0; 2048];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

/// A listener that counts connections and closes each one at once, so a
/// page that does reach it fails fast instead of hanging.
async fn counting_listener() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reached = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reached);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (port, reached)
}

fn reference(snapshot: &str, line_start: &str) -> String {
    let line = snapshot
        .lines()
        .find(|line| line.trim_start().starts_with(line_start))
        .unwrap_or_else(|| panic!("{line_start} missing from:\n{snapshot}"));
    let start = line.find("[ref=").expect("ref") + 5;
    line[start..line[start..].find(']').unwrap() + start].to_owned()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn agent_reads_and_operates_a_page_by_ref_under_the_request_policy() {
    // Given: a local page whose image points at a host the policy refuses.
    let url = serve(PAGE.to_owned()).await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 2,
        max_tabs: 4,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");

    // When: the agent navigates, reads the outline and acts on its refs.
    page.navigate(&url).await.expect("navigate");
    let snapshot = page.snapshot().await.unwrap().text;
    assert!(snapshot.contains("heading \"Agent fixture\""), "{snapshot}");
    page.type_text(&reference(&snapshot, "- textbox \"Name\""), "Ada", false)
        .await
        .unwrap();
    page.click(&reference(&snapshot, "- button \"Greet\""))
        .await
        .unwrap();
    let after = page.snapshot().await.unwrap().text;
    let png = page.screenshot().await.unwrap();

    // Then: the page reacted, the screenshot is a PNG and the image was refused.
    assert!(after.contains("Hello Ada"), "{after}");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let blocked = page.take_blocked();
    assert!(
        blocked
            .iter()
            .any(|entry| entry.contains("blocked.invalid")),
        "{blocked:?}"
    );
    assert!(page.navigate("http://blocked.invalid/").await.is_err());
    assert!(browser.close("run-1").await);
    assert!(!browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn page_websockets_and_worker_fetches_cannot_reach_a_refused_destination() {
    // Given: a loopback port the policy refuses and a page whose script
    // targets it from a WebSocket and from a dedicated worker.
    let (port, reached) = counting_listener().await;
    let target = format!("127.0.0.1:{port}");
    let url = serve(format!(
        r#"<!doctype html><html><body><script>
window.attempts = Promise.all([
  new Promise(done => {{
    const socket = new WebSocket("ws://{target}/");
    socket.onopen = () => done("open");
    socket.onerror = () => done("error");
  }}),
  new Promise(done => {{
    const source = `fetch("http://{target}/").then(() => postMessage("ok"), () => postMessage("error"))`;
    const worker = new Worker(URL.createObjectURL(new Blob([source])));
    worker.onmessage = event => done(event.data);
  }}),
]);
</script></body></html>"#
    ))
    .await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port,
        }),
        max_pages: 1,
        max_tabs: 4,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");

    // When: the page loads and both attempts settle.
    page.navigate(&url).await.expect("navigate");
    let settled = page
        .target()
        .call(
            "Runtime.evaluate",
            json!({"expression": "window.attempts", "awaitPromise": true, "returnByValue": true}),
        )
        .await
        .unwrap();

    // Then: both failed and no connection reached the refused port.
    assert_eq!(
        settled["result"]["value"],
        json!(["error", "error"]),
        "{settled}"
    );
    assert_eq!(reached.load(Ordering::SeqCst), 0);
    assert!(browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn observers_follow_pages_through_their_screencast_and_activity() {
    // Given: an agent browser whose page list is being watched.
    let url = serve(PAGE.to_owned()).await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 2,
        max_tabs: 4,
    });
    let mut pages = browser.pages();
    assert!(pages.borrow_and_update().is_empty());

    // When: a run opens a page and works on it under a screencast.
    let page = browser.page("run-1").await.expect("launch Chromium");
    let opened = pages.borrow_and_update().clone();
    let mut screencast = browser.screencast("run-1", Arc::new(|| ())).await.unwrap();
    page.navigate(&url).await.unwrap();
    let frame = screencast.next_frame().await.expect("a frame");
    let activity = screencast.agent().activity();
    let switches = page
        .target()
        .call("Browser.getBrowserCommandLine", json!({}))
        .await
        .unwrap();
    let viewport = page
        .target()
        .call(
            "Runtime.evaluate",
            json!({"expression": "[innerWidth, navigator.maxTouchPoints]", "returnByValue": true}),
        )
        .await
        .unwrap();
    let missing = browser.screencast("run-2", Arc::new(|| ())).await;
    assert!(browser.close("run-1").await);
    pages.changed().await.unwrap();

    // Then: observers saw the page open and close, its frames and actions,
    // the page kept the window's viewport, and Chromium received the guard
    // switches.
    assert_eq!(opened, ["run-1"]);
    assert!(pages.borrow().is_empty());
    assert!(frame.width() > 0 && frame.height() > 0);
    assert!(
        activity.contains(&Activity::Action {
            label: format!("navigate {url}"),
            error: None
        }),
        "{activity:?}"
    );
    assert!(
        activity
            .iter()
            .any(|entry| matches!(entry, Activity::Blocked(request) if request.contains("blocked.invalid"))),
        "{activity:?}"
    );
    assert!(matches!(missing, Err(AgentError::NoPage(_))));
    assert_eq!(
        viewport["result"]["value"],
        json!([1280, 0]),
        "the window's own width, without emulated touch"
    );
    let switches: Vec<&str> = switches["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|argument| argument.as_str())
        .collect();
    for switch in [
        "--proxy-bypass-list=<-loopback>",
        "--disable-quic",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    ] {
        assert!(switches.contains(&switch), "{switch}: {switches:?}");
    }
    assert!(
        switches
            .iter()
            .any(|argument| argument.starts_with("--proxy-server=http://127.0.0.1:")),
        "{switches:?}"
    );
    assert!(
        switches
            .iter()
            .any(|argument| argument.starts_with("--disable-features=")
                && argument.contains("BackForwardCache")
                && argument.contains("site-per-process")),
        "{switches:?}"
    );
    while screencast.next_frame().await.is_some() {}
    assert!(screencast.ended(), "the screencast ends with its page");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn click_at_hits_what_is_drawn_at_the_viewport_point() {
    // Given: a page whose button covers the top-left quarter of the viewport.
    let url = serve(
        r#"<!doctype html><html><body style="margin:0">
<button style="position:fixed;left:0;top:0;width:50vw;height:50vh"
  onclick="this.textContent = 'Pressed'">Press</button>
</body></html>"#
            .to_owned(),
    )
    .await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 1,
        max_tabs: 4,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");
    page.navigate(&url).await.unwrap();

    // When: the agent clicks outside the button, then inside it.
    page.click_at(1000.0, 700.0).await.unwrap();
    let missed = page.snapshot().await.unwrap().text;
    page.click_at(20.0, 20.0).await.unwrap();
    let hit = page.snapshot().await.unwrap().text;

    // Then: only the click inside it pressed the button.
    assert!(missed.contains("button \"Press\""), "{missed}");
    assert!(hit.contains("button \"Pressed\""), "{hit}");
    assert!(browser.close("run-1").await);
}

const CONTROLS: &str = r##"<!doctype html><html><body>
<h1 id="where"></h1>
<script>
const show = () => document.getElementById("where").textContent = "At " + location.pathname + location.hash;
show();
addEventListener("hashchange", show);
</script>
<a href="#details">Details</a>
<button onmouseenter="document.getElementById('items').hidden = false">Menu</button>
<ul id="items" hidden><li>Hidden item</li></ul>
<label>Size <select onchange="document.getElementById('picked').textContent = 'Picked ' + this.value">
<option value="s">Small</option><option value="l">Large</option></select></label>
<p id="picked">Picked nothing</p>
<button onclick="document.getElementById('answer').textContent = 'Confirm ' + confirm('Delete it?')">Delete</button>
<button onclick="document.getElementById('answer').textContent = 'Prompt ' + prompt('Name?', 'nobody')">Name</button>
<button onclick="alert('Saved'); document.getElementById('answer').textContent = 'Alerted'">Save</button>
<p id="answer">Unanswered</p>
</body></html>"##;

async fn controls_page(browser: &AgentBrowser) -> (Arc<PageAgent>, String) {
    let url = serve(CONTROLS.to_owned()).await;
    let page = browser.page("run-1").await.expect("launch Chromium");
    (page, url)
}

fn controls_browser() -> AgentBrowser {
    AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 1,
        max_tabs: 4,
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn history_hover_and_select_act_like_a_user() {
    // Given: two documents in history and a same-document entry after them.
    let browser = controls_browser();
    let (page, url) = controls_page(&browser).await;
    page.navigate(&format!("{url}one")).await.unwrap();
    page.navigate(&format!("{url}two")).await.unwrap();
    let outline = page.snapshot().await.unwrap().text;
    page.click(&reference(&outline, "- link \"Details\""))
        .await
        .unwrap();

    // When: the agent walks the history, hovers the menu and picks an option.
    let mut visited = Vec::new();
    for step in ["back", "back", "forward", "forward"] {
        match step {
            "back" => page.go_back().await.unwrap(),
            _ => page.go_forward().await.unwrap(),
        }
        let text = page.snapshot().await.unwrap().text;
        visited.push(
            text.lines()
                .find(|line| line.contains("At /"))
                .and_then(|line| line.trim().split(" [ref=").next())
                .unwrap_or_default()
                .to_owned(),
        );
    }
    let past_end = page.go_forward().await;
    let outline = page.snapshot().await.unwrap().text;
    page.hover(&reference(&outline, "- button \"Menu\""))
        .await
        .unwrap();
    let hovered = page.snapshot().await.unwrap().text;
    let size = reference(&hovered, "- combobox \"Size\"");
    page.select_option(&size, &["Large".into()]).await.unwrap();
    let picked = page.snapshot().await.unwrap().text;
    let missing = page.select_option(&size, &["Huge".into()]).await;

    // Then: each step showed the expected entry and the page reacted.
    assert_eq!(
        visited,
        [
            "- heading \"At /two\"",
            "- heading \"At /one\"",
            "- heading \"At /two\"",
            "- heading \"At /two#details\"",
        ],
        "{visited:?}"
    );
    assert!(
        matches!(past_end, Err(AgentError::Refused(_))),
        "{past_end:?}"
    );
    assert!(!outline.contains("Hidden item"), "{outline}");
    assert!(hovered.contains("Hidden item"), "{hovered}");
    assert!(picked.contains("Picked l"), "{picked}");
    assert!(
        matches!(&missing, Err(AgentError::Refused(reason)) if reason.contains("\"Small\",\"Large\"")),
        "{missing:?}"
    );
    assert!(browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn dialogs_are_dismissed_unless_the_agent_answers_them() {
    // Given: a page whose buttons open a confirm, a prompt and an alert.
    let browser = controls_browser();
    let (page, url) = controls_page(&browser).await;
    page.navigate(&url).await.unwrap();
    let outline = page.snapshot().await.unwrap().text;
    let delete = reference(&outline, "- button \"Delete\"");
    let name = reference(&outline, "- button \"Name\"");
    let save = reference(&outline, "- button \"Save\"");
    let answer = |page: Arc<PageAgent>| async move {
        let text = page.snapshot().await.unwrap().text;
        text.lines()
            .find(|line| {
                line.contains("Confirm ") || line.contains("Prompt ") || line.contains("Alerted")
            })
            .unwrap_or_default()
            .trim()
            .to_owned()
    };

    // When: the agent clicks each with and without an answer.
    page.click(&delete).await.unwrap();
    let unanswered_confirm = answer(page.clone()).await;
    page.answer_next_dialog(DialogAnswer {
        accept: true,
        prompt_text: None,
    });
    page.click(&delete).await.unwrap();
    let accepted_confirm = answer(page.clone()).await;
    page.click(&name).await.unwrap();
    let unanswered_prompt = answer(page.clone()).await;
    page.answer_next_dialog(DialogAnswer {
        accept: true,
        prompt_text: Some("Ada".into()),
    });
    page.click(&name).await.unwrap();
    let answered_prompt = answer(page.clone()).await;
    page.click(&save).await.unwrap();
    let alerted = answer(page.clone()).await;

    // Then: only the answered dialogs were accepted, and each was noted.
    assert_eq!(
        [
            unanswered_confirm,
            accepted_confirm,
            unanswered_prompt,
            answered_prompt,
            alerted,
        ],
        [
            "- text \"Confirm false\"",
            "- text \"Confirm true\"",
            "- text \"Prompt null\"",
            "- text \"Prompt Ada\"",
            "- text \"Alerted\"",
        ]
        .map(String::from)
    );
    let dialogs = page.take_dialogs();
    assert_eq!(dialogs.len(), 5, "{dialogs:?}");
    assert!(
        dialogs[0].starts_with("confirm \"Delete it?\" dismissed"),
        "{dialogs:?}"
    );
    assert_eq!(dialogs[1], "confirm \"Delete it?\" accepted");
    assert_eq!(dialogs[3], "prompt \"Name?\" answered \"Ada\"");
    assert_eq!(dialogs[4], "alert \"Saved\" closed");
    assert!(browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn uploaded_files_reach_the_page_through_its_file_input() {
    // Given: a page that reads whatever its file input receives.
    let url = serve(
        r#"<!doctype html><html><body>
<label>Attachment <input type="file" multiple></label>
<button>Other</button>
<script>
const input = document.querySelector("input");
window.received = new Promise(resolve => input.addEventListener("change", async () => {
  resolve(await Promise.all([...input.files].map(async file => file.name + "=" + await file.text())));
}));
</script>
</body></html>"#
            .to_owned(),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let files = ["a.txt", "b.txt"].map(|name| {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("contents of {name}")).unwrap();
        path.to_string_lossy().into_owned()
    });
    let browser = controls_browser();
    let page = browser.page("run-1").await.expect("launch Chromium");
    page.navigate(&url).await.unwrap();
    let outline = page.snapshot().await.unwrap().text;

    // When: the agent asks where uploads would go, then uploads two files.
    let input = reference(&outline, "- button \"Attachment\"");
    let target = page.upload_target(&input).await.unwrap();
    let not_an_input = page
        .upload_target(&reference(&outline, "- button \"Other\""))
        .await;
    page.upload_files(&input, &files).await.unwrap();
    let received = page
        .target()
        .call(
            "Runtime.evaluate",
            json!({"expression": "window.received", "awaitPromise": true, "returnByValue": true}),
        )
        .await
        .unwrap();

    // Then: the target is the input's document, and the page's change
    // listener read both files.
    assert_eq!(
        (target.frame_url.as_str(), target.page_url.as_str()),
        (url.as_str(), url.as_str())
    );
    assert!(
        matches!(not_an_input, Err(AgentError::Refused(_))),
        "{not_an_input:?}"
    );
    assert_eq!(
        received["result"]["value"],
        json!(["a.txt=contents of a.txt", "b.txt=contents of b.txt"]),
        "{received}"
    );
    assert!(browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn popups_open_as_tabs_of_their_run_under_the_same_guard() {
    // Given: a refused loopback port, and a page that opens tabs to it with
    // window.open and a target=_blank link, and to an allowed page. Pages of
    // the fixture other than the first report their load with a request to
    // the refused port.
    let (port, reached) = counting_listener().await;
    let refused = format!("http://127.0.0.1:{port}");
    let url = serve(format!(
        r#"<!doctype html><html><body>
<h1 id="where"></h1>
<script>
document.getElementById("where").textContent = "At " + location.pathname;
if (location.pathname !== "/") fetch("{refused}/loaded" + location.pathname).catch(() => {{}});
</script>
<button onclick="window.open('{refused}/scripted')">Open refused</button>
<a href="{refused}/linked" target="_blank">Refused link</a>
<a href="/second" target="_blank">Second</a>
<button onclick="window.open('/third')">Open third</button>
</body></html>"#
    ))
    .await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port,
        }),
        max_pages: 8,
        max_tabs: 4,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");
    page.navigate(&url).await.unwrap();
    let outline = page.snapshot().await.unwrap().text;

    // When: the agent opens each tab, then one more than its run may hold,
    // and works in the new tabs.
    let mut news = Vec::new();
    for name in [
        "- button \"Open refused\"",
        "- link \"Refused link\"",
        "- link \"Second\"",
        "- button \"Open third\"",
    ] {
        page.click(&reference(&outline, name)).await.unwrap();
        news.push(browser.tab_news("run-1"));
    }
    let listed = browser.tabs("run-1").await.unwrap();
    let mut refusals = Vec::new();
    for id in ["t2", "t3"] {
        let tab = browser.switch_tab("run-1", id).await.unwrap();
        let mut changes = tab.activity_changes();
        changes
            .wait_for(|_| {
                tab.activity()
                    .iter()
                    .any(|entry| matches!(entry, Activity::Blocked(request) if request.contains(&refused)))
            })
            .await
            .unwrap();
        refusals.push(tab.take_blocked());
    }
    let second = browser.switch_tab("run-1", "t4").await.unwrap();
    let mut changes = second.activity_changes();
    changes
        .wait_for(|_| {
            second
                .activity()
                .iter()
                .any(|entry| matches!(entry, Activity::Blocked(request) if request.contains("/loaded/second")))
        })
        .await
        .unwrap();
    let second_outline = second.snapshot().await.unwrap().text;
    browser.close_tab("run-1", "t4").await.unwrap();
    let after_close = browser.tabs("run-1").await.unwrap();
    let missing = browser.switch_tab("run-1", "t5").await.err();

    // Then: every tab joined the run, the refused destination was never
    // reached from either, the extra popup was closed, and closing the
    // active tab returned to its opener.
    let first_news: Vec<&str> = news
        .iter()
        .map(|entries| entries.first().map(String::as_str).unwrap_or_default())
        .collect();
    assert!(first_news[0].starts_with("t2 opened from t1"), "{news:?}");
    assert!(first_news[1].starts_with("t3 opened from t1"), "{news:?}");
    assert!(first_news[2].starts_with("t4 opened from t1"), "{news:?}");
    assert!(first_news[3].contains("already has 4 tabs"), "{news:?}");
    let summary = |tabs: &[browser::agent::TabInfo]| -> Vec<String> {
        tabs.iter()
            .map(|tab| format!("{}{}", tab.id, if tab.active { "*" } else { "" }))
            .collect()
    };
    assert_eq!(summary(&listed), ["t1*", "t2", "t3", "t4"], "{listed:?}");
    assert_eq!(listed[1].opener.as_deref(), Some("t1"));
    assert_eq!(listed[0].url, url);
    for blocked in &refusals {
        assert!(
            blocked.iter().any(|entry| entry.contains(&refused)),
            "{refusals:?}"
        );
    }
    assert_eq!(reached.load(Ordering::SeqCst), 0);
    assert!(second_outline.contains("At /second"), "{second_outline}");
    assert_eq!(
        summary(&after_close),
        ["t1*", "t2", "t3"],
        "{after_close:?}"
    );
    assert!(
        matches!(&missing, Some(AgentError::Refused(reason)) if reason.contains("open tabs: t1, t2, t3")),
        "{missing:?}"
    );
    assert!(browser.close("run-1").await);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn the_screencast_follows_the_runs_active_tab() {
    // Given: a run's page under a screencast, with a link that opens a tab.
    let url = serve(
        r#"<!doctype html><html><body><a href="/second" target="_blank">Second</a></body></html>"#
            .to_owned(),
    )
    .await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 2,
        max_tabs: 2,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");
    page.navigate(&url).await.unwrap();
    let mut screencast = browser.screencast("run-1", Arc::new(|| ())).await.unwrap();
    let first = screencast.tab();
    let outline = page.snapshot().await.unwrap().text;

    // When: the agent opens the tab, moves to it and paints it blue, then
    // closes it.
    page.click(&reference(&outline, "- link \"Second\""))
        .await
        .unwrap();
    let second = browser.switch_tab("run-1", "t2").await.unwrap();
    let followed = screencast.next_tab().await;
    let observed_second = Arc::ptr_eq(&screencast.agent(), &second);
    second.navigate(&format!("{url}blue")).await.unwrap();
    second
        .target()
        .call(
            "Runtime.evaluate",
            json!({"expression": "document.documentElement.style.background = 'rgb(0, 0, 255)'"}),
        )
        .await
        .unwrap();
    let blue = loop {
        let frame = screencast.next_frame().await.expect("frames of t2");
        let [r, g, b, _] = frame.get_pixel(frame.width() / 2, frame.height() / 2).0;
        if r < 40 && g < 40 && b > 200 {
            break true;
        }
    };
    browser.close_tab("run-1", "t2").await.unwrap();
    let returned = screencast.next_tab().await;
    let observed_first = Arc::ptr_eq(&screencast.agent(), &page);
    assert!(browser.close("run-1").await);
    let after_close = screencast.next_tab().await;

    // Then: the screencast showed t1, then t2's frames, then t1 again, and
    // ended with the run.
    assert_eq!(first, "t1");
    assert_eq!(followed.as_deref(), Some("t2"));
    assert!(observed_second);
    assert!(blue);
    assert_eq!(returned.as_deref(), Some("t1"));
    assert!(observed_first);
    assert_eq!(after_close, None);
    while screencast.next_frame().await.is_some() {}
    assert!(screencast.ended());
}

const LATE: &str = r#"<!doctype html><html><head><title>Late</title></head><body>
<p id="status">Loading</p>
<script>
window.reveal = () => {
  document.getElementById('status').textContent = 'Ready';
  const button = document.createElement('button');
  button.textContent = 'Continue';
  button.onclick = () => { document.getElementById('status').textContent = 'Continued'; };
  document.body.append(button);
};
window.hold = () => fetch('/hold').then(() => { document.title = 'Held'; });
</script>
</body></html>"#;

/// Serves `page`, and answers `/hold` only once `release` is notified.
async fn serve_holding(page: &'static str, release: Arc<tokio::sync::Notify>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                let mut request = [0; 2048];
                let read = socket.read(&mut request).await.unwrap_or_default();
                let body = if request[..read].starts_with(b"GET /hold ") {
                    release.notified().await;
                    "held"
                } else {
                    page
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

/// Resolves once `page` has checked the conditions of `label` once and
/// started waiting for the page to change.
async fn waiting(page: &PageAgent, label: &str) {
    let label = format!("waiting for {label}");
    page.activity_changes()
        .wait_for(|_| {
            page.activity().iter().any(
                |entry| matches!(entry, Activity::Action { label: seen, .. } if *seen == label),
            )
        })
        .await
        .unwrap();
}

/// Runs `trigger` in the page once the wait has seen the page without what
/// it waits for, and returns how the wait ended.
async fn wait_then_trigger(
    page: &Arc<PageAgent>,
    wanted: WaitFor,
    label: &str,
    trigger: &str,
) -> Waited {
    let wait = tokio::spawn({
        let page = Arc::clone(page);
        async move {
            // No time limit: only page events decide the outcome here.
            let limits = WaitLimits {
                events: 1000,
                time: std::time::Duration::MAX,
            };
            page.wait_for(&wanted, limits).await
        }
    });
    waiting(page, label).await;
    page.target()
        .call("Runtime.evaluate", json!({"expression": trigger}))
        .await
        .unwrap();
    wait.await.unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium; run explicitly with --ignored"]
async fn waits_end_on_the_page_events_they_name() {
    // Given: a page that shows its content only when the test triggers it,
    // and holds a request until the test releases it.
    let release = Arc::new(tokio::sync::Notify::new());
    let url = serve_holding(LATE, Arc::clone(&release)).await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(Refuse {
            host: "blocked.invalid",
            port: 0,
        }),
        max_pages: 1,
        max_tabs: 1,
    });
    let page = browser.page("run-1").await.expect("launch Chromium");
    page.navigate(&url).await.expect("navigate");
    let before = page.snapshot().await.unwrap().text;
    assert!(!before.contains("Continue"), "{before}");

    // When: the agent waits for the content, then uses it.
    let appeared = wait_then_trigger(
        &page,
        WaitFor {
            text: Some("Ready".into()),
            ..Default::default()
        },
        "text \"Ready\" to appear",
        "reveal()",
    )
    .await;
    let shown = page.snapshot().await.unwrap().text;
    let button = reference(&shown, "- button \"Continue\"");
    page.click(&button)
        .await
        .expect("click the revealed button");
    let clicked = page.snapshot().await.unwrap().text;

    // Then: the wait saw the page without it first, and the ref works.
    assert_eq!(appeared.outcome, WaitOutcome::Met, "{appeared:?}");
    assert!(appeared.checks >= 2, "{appeared:?}");
    assert!(clicked.contains("Continued"), "{clicked}");

    // When: the agent waits for the button to go and for the URL to change.
    let gone = wait_then_trigger(
        &page,
        WaitFor {
            reference_gone: Some(button.clone()),
            ..Default::default()
        },
        &format!("{button} to disappear"),
        "document.querySelector('button').remove()",
    )
    .await;
    let moved = wait_then_trigger(
        &page,
        WaitFor {
            url: Some("/done".into()),
            url_changes: true,
            ..Default::default()
        },
        "a URL containing \"/done\", the URL to change",
        "history.pushState(null, '', '/done')",
    )
    .await;

    // Then: each wait ended on the change it named.
    for waited in [&gone, &moved] {
        assert_eq!(waited.outcome, WaitOutcome::Met, "{waited:?}");
        assert!(waited.checks >= 2, "{waited:?}");
    }

    // When: a request is in flight, the agent waits for the network to go
    // quiet, and the server answers.
    page.target()
        .call("Runtime.evaluate", json!({"expression": "hold(); 0"}))
        .await
        .unwrap();
    page.target()
        .page_events()
        .await
        .unwrap()
        .expect("Chromium reports page events")
        .wait_for(|seen| seen.requests > 0)
        .await
        .unwrap();
    let wait = tokio::spawn({
        let page = Arc::clone(&page);
        async move {
            let wanted = WaitFor {
                network_idle: true,
                ..Default::default()
            };
            let limits = WaitLimits {
                events: 1000,
                time: std::time::Duration::MAX,
            };
            page.wait_for(&wanted, limits).await
        }
    });
    waiting(&page, "no request in flight").await;
    release.notify_one();
    let quiet = wait.await.unwrap().unwrap();

    // Then: the wait ended once the held request finished.
    assert_eq!(quiet.outcome, WaitOutcome::Met, "{quiet:?}");
    assert!(quiet.checks >= 2, "{quiet:?}");
}
