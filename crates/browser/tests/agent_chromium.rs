use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use browser::agent::{
    Activity, AgentBrowser, AgentError, LaunchOptions, PageProvider, RequestPolicy,
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
    let missing = browser.screencast("run-2", Arc::new(|| ())).await;
    assert!(browser.close("run-1").await);
    pages.changed().await.unwrap();

    // Then: observers saw the page open and close, its frames and actions,
    // and Chromium received the guard switches.
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
    let switches: Vec<&str> = switches["arguments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|argument| argument.as_str())
        .collect();
    for switch in [
        "--proxy-bypass-list=<-loopback>",
        "--disable-quic",
        "--block-new-web-contents",
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
