use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use base64::Engine;
use browser::agent::CdpTarget;
use serde_json::Value;

use super::*;
use crate::network_guard::{DnsResolver, NetworkGuardError};

#[derive(Default)]
struct FakePage {
    calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl CdpTarget for FakePage {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, AgentError> {
        self.calls.lock().unwrap().push(method.to_owned());
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "link"}, "name": {"value": "Docs"},
                "backendDOMNodeId": 5
            }]}),
            "Runtime.evaluate" => json!({"result": {"value": "https://example.test/"}}),
            "DOM.getContentQuads" => json!({"quads": [[0, 0, 10, 0, 10, 10, 0, 10]]}),
            "Page.captureScreenshot" => {
                let mut png = std::io::Cursor::new(Vec::new());
                image::RgbImage::new(3, 2)
                    .write_to(&mut png, image::ImageFormat::Png)
                    .unwrap();
                json!({"data": base64::engine::general_purpose::STANDARD.encode(png.into_inner())})
            }
            _ => json!({}),
        })
    }

    async fn navigate(&self, url: &str) -> Result<(), AgentError> {
        self.calls.lock().unwrap().push(format!("navigate {url}"));
        Ok(())
    }
}

type OpenPage = (Arc<FakePage>, Arc<PageAgent>);

#[derive(Default)]
struct FakeProvider {
    pages: tokio::sync::Mutex<HashMap<String, OpenPage>>,
}

impl FakeProvider {
    async fn calls(&self, key: &str) -> Vec<String> {
        self.pages.lock().await[key].0.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl PageProvider for FakeProvider {
    async fn page(&self, key: &str) -> Result<Arc<PageAgent>, AgentError> {
        let mut pages = self.pages.lock().await;
        let (_, agent) = pages.entry(key.to_owned()).or_insert_with(|| {
            let page = Arc::new(FakePage::default());
            let agent = Arc::new(PageAgent::new(page.clone()));
            (page, agent)
        });
        Ok(Arc::clone(agent))
    }

    async fn close(&self, key: &str) -> bool {
        self.pages.lock().await.remove(key).is_some()
    }

    fn release(&self, _key: &str) {}
}

fn tool(provider: &Arc<FakeProvider>, name: &str) -> BrowserTool {
    browser_tools(
        provider.clone() as Arc<dyn PageProvider>,
        BrowserToolOptions {
            coordinate_actions: true,
        },
    )
    .into_iter()
    .find(|tool| tool.name() == name)
    .unwrap()
}

fn context(run_id: &str) -> ToolExecutionContext {
    ToolExecutionContext {
        run_id: run_id.into(),
        thread_id: None,
        call_id: None,
    }
}

#[test]
fn browser_tool_schemas_compile_with_unique_names() {
    let all = BrowserToolOptions {
        coordinate_actions: true,
    };
    let tools = browser_tools(Arc::new(FakeProvider::default()), all);
    let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(names, BROWSER_TOOL_NAMES);
    let default: Vec<_> = browser_tools(Arc::new(FakeProvider::default()), Default::default())
        .iter()
        .map(|tool| tool.name().to_owned())
        .collect();
    assert!(!default.iter().any(|name| name == "browser_click_at"));
    assert_eq!(default.len(), BROWSER_TOOL_NAMES.len() - 1);
    for tool in &tools {
        jsonschema::validator_for(&tool.schema())
            .unwrap_or_else(|error| panic!("{}: {error}", tool.name()));
        assert_eq!(tool.permissions(), Permissions::network());
    }
}

#[tokio::test]
async fn pages_are_per_run_and_actions_return_the_outline() {
    let provider = Arc::new(FakeProvider::default());

    let opened = tool(&provider, "browser_navigate")
        .execute_with_context(&context("run-a"), json!({"url": "http://example.test/"}))
        .await
        .unwrap();
    let stale = tool(&provider, "browser_click")
        .execute_with_context(&context("run-b"), json!({"ref": "e5"}))
        .await
        .unwrap();
    let clicked = tool(&provider, "browser_click")
        .execute_with_context(&context("run-a"), json!({"ref": "e5"}))
        .await
        .unwrap();

    assert!(!opened.is_error);
    assert!(
        opened.content.contains("- link \"Docs\" [ref=e5]"),
        "{}",
        opened.content
    );
    assert_eq!(
        provider.calls("run-a").await.first().map(String::as_str),
        Some("navigate https://example.test/")
    );
    assert!(stale.is_error, "run-b has its own page without refs");
    assert!(!clicked.is_error, "{}", clicked.content);
    assert!(
        tool(&provider, "browser_close")
            .execute_with_context(&context("run-a"), json!({}))
            .await
            .unwrap()
            .content
            .contains("closed")
    );
}

#[tokio::test]
async fn screenshots_are_image_results_and_bad_arguments_are_rejected() {
    let provider = Arc::new(FakeProvider::default());

    let shot = tool(&provider, "browser_screenshot")
        .execute_with_context(&context("run-a"), json!({}))
        .await
        .unwrap();
    let invalid = tool(&provider, "browser_click")
        .execute_with_context(&context("run-a"), json!({"selector": "#go"}))
        .await;

    assert_eq!(shot.images.len(), 1);
    assert_eq!(shot.images[0].media_type, "image/png");
    assert!(matches!(invalid, Err(ToolError::InvalidArgs { .. })));
}

struct FixedDns(Vec<IpAddr>);

#[async_trait::async_trait]
impl DnsResolver for FixedDns {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn guard_policy_applies_web_fetch_destination_rules() {
    let policy = |addr: &str| {
        GuardPolicy(Arc::new(NetworkGuard::with_resolver(Arc::new(FixedDns(
            vec![addr.parse().unwrap()],
        )))))
    };
    let url = |text: &str| url::Url::parse(text).unwrap();

    assert!(
        policy("203.0.113.5")
            .check(&url("https://example.test/a"))
            .await
            .is_ok()
    );
    assert!(
        policy("203.0.113.5")
            .check(&url("http://example.test/a"))
            .await
            .is_err()
    );
    assert!(
        policy("169.254.169.254")
            .check(&url("https://metadata.test/"))
            .await
            .is_err()
    );
    assert!(
        policy("203.0.113.5")
            .check(&url("ftp://example.test/"))
            .await
            .is_err()
    );
    assert_eq!(
        policy("203.0.113.5")
            .resolve("example.test", 8443, true)
            .await,
        Ok(vec!["203.0.113.5:8443".parse().unwrap()])
    );
    assert!(
        policy("203.0.113.5")
            .resolve("example.test", 80, false)
            .await
            .is_err()
    );
    assert!(
        policy("169.254.169.254")
            .resolve("metadata.test", 443, true)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn click_at_presses_the_viewport_point_and_rejects_points_outside_it() {
    let provider = Arc::new(FakeProvider::default());
    let click_at = tool(&provider, "browser_click_at");

    let clicked = click_at
        .execute_with_context(&context("run-a"), json!({"x": 40, "y": 12.5}))
        .await
        .unwrap();
    let outside = click_at
        .execute_with_context(&context("run-a"), json!({"x": -1, "y": 3}))
        .await
        .unwrap();

    assert!(!clicked.is_error, "{}", clicked.content);
    assert!(
        clicked.content.contains("- link \"Docs\""),
        "{}",
        clicked.content
    );
    let calls = provider.calls("run-a").await;
    assert_eq!(
        calls
            .iter()
            .filter(|call| *call == "Input.dispatchMouseEvent")
            .count(),
        3
    );
    assert!(outside.is_error);
}
