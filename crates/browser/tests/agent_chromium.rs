use std::sync::Arc;

use browser::agent::{AgentBrowser, LaunchOptions, PageProvider, RequestPolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PAGE: &str = r#"<!doctype html><html><head><title>Agent fixture</title></head><body>
<h1>Agent fixture</h1>
<img src="http://blocked.invalid/pixel.png" alt="">
<label>Name <input id="name"></label>
<button onclick="document.getElementById('out').textContent = 'Hello ' + document.getElementById('name').value">Greet</button>
<p id="out">waiting</p>
</body></html>"#;

struct BlockHost(&'static str);

#[async_trait::async_trait]
impl RequestPolicy for BlockHost {
    async fn check(&self, url: &url::Url) -> Result<(), String> {
        if url.host_str() == Some(self.0) {
            Err("blocked by test policy".into())
        } else {
            Ok(())
        }
    }
}

async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 2048];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
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
    let url = serve().await;
    let browser = AgentBrowser::new(LaunchOptions {
        executable: None,
        policy: Arc::new(BlockHost("blocked.invalid")),
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
