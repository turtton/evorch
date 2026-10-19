use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

use base64::Engine;
use browser::agent::{CdpTarget, TabInfo};
use serde_json::Value;

use super::*;
use crate::network_guard::{DnsResolver, NetworkGuardError};

#[derive(Default)]
struct FakePage {
    calls: Mutex<Vec<String>>,
    uploaded: Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl CdpTarget for FakePage {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.calls.lock().unwrap().push(method.to_owned());
        if method == "DOM.setFileInputFiles" {
            self.uploaded.lock().unwrap().push(params["files"].clone());
        }
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "link"}, "name": {"value": "Docs"},
                "backendDOMNodeId": 5
            }]}),
            "Runtime.evaluate" => json!({"result": {"value": "https://example.test/"}}),
            "Runtime.callFunctionOn"
                if params["functionDeclaration"]
                    .as_str()
                    .is_some_and(|function| function.contains("ownerDocument")) =>
            {
                json!({"result": {"value": "https://forms.example.test/upload?step=2"}})
            }
            "DOM.getContentQuads" => json!({"quads": [[0, 0, 10, 0, 10, 10, 0, 10]]}),
            "DOM.resolveNode" => json!({"object": {"objectId": "obj-5"}}),
            "Page.getNavigationHistory" => json!({
                "currentIndex": 1,
                "entries": [{"id": 1}, {"id": 2}],
            }),
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
    tabs: Mutex<Vec<TabInfo>>,
    news: Mutex<Vec<String>>,
}

impl FakeProvider {
    async fn calls(&self, key: &str) -> Vec<String> {
        self.pages.lock().await[key].0.calls.lock().unwrap().clone()
    }

    async fn uploaded(&self, key: &str) -> Vec<Value> {
        self.pages.lock().await[key]
            .0
            .uploaded
            .lock()
            .unwrap()
            .clone()
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

    async fn open_page(&self, key: &str) -> Option<Arc<PageAgent>> {
        self.pages
            .lock()
            .await
            .get(key)
            .map(|(_, agent)| Arc::clone(agent))
    }

    async fn close(&self, key: &str) -> bool {
        self.pages.lock().await.remove(key).is_some()
    }

    fn release(&self, _key: &str) {}

    async fn tabs(&self, _key: &str) -> Result<Vec<TabInfo>, AgentError> {
        Ok(self.tabs.lock().unwrap().clone())
    }

    async fn switch_tab(&self, key: &str, id: &str) -> Result<Arc<PageAgent>, AgentError> {
        {
            let mut tabs = self.tabs.lock().unwrap();
            if !tabs.iter().any(|tab| tab.id == id) {
                return Err(AgentError::Refused(format!("no tab {id}")));
            }
            for tab in tabs.iter_mut() {
                tab.active = tab.id == id;
            }
        }
        self.page(key).await
    }

    async fn close_tab(&self, _key: &str, id: &str) -> Result<(), AgentError> {
        let mut tabs = self.tabs.lock().unwrap();
        tabs.retain(|tab| tab.id != id);
        if let Some(first) = tabs.first_mut() {
            first.active = true;
        }
        Ok(())
    }

    fn tab_news(&self, _key: &str) -> Vec<String> {
        std::mem::take(&mut self.news.lock().unwrap())
    }
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
    assert!(default.iter().any(|name| name == "browser_handle_dialog"));
    assert_eq!(default.len(), BROWSER_TOOL_NAMES.len() - 1);
    for tool in &tools {
        jsonschema::validator_for(&tool.schema())
            .unwrap_or_else(|error| panic!("{}: {error}", tool.name()));
        if tool.name() != "browser_upload_file" {
            assert_eq!(tool.permissions(), Permissions::network());
        }
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

#[tokio::test]
async fn page_operations_return_the_outline_and_dialogs_are_listed() {
    let provider = Arc::new(FakeProvider::default());
    let run = |name: &str, args: Value| {
        let tool = tool(&provider, name);
        async move {
            tool.execute_with_context(&context("run-a"), args)
                .await
                .unwrap()
        }
    };
    run("browser_snapshot", json!({})).await;

    let back = run("browser_navigate_back", json!({})).await;
    let forward = run("browser_navigate_forward", json!({})).await;
    run("browser_snapshot", json!({})).await;
    let hover = run("browser_hover", json!({"ref": "e5"})).await;
    let selected = run(
        "browser_select_option",
        json!({"ref": "e5", "values": ["Large"]}),
    )
    .await;
    let armed = run("browser_handle_dialog", json!({"accept": true})).await;
    let page = provider.page("run-a").await.unwrap();
    let answer = page.on_dialog(browser::agent::DialogKind::Confirm, "Delete it?", None);
    let listed = run("browser_snapshot", json!({})).await;
    let invalid = tool(&provider, "browser_select_option")
        .execute_with_context(&context("run-a"), json!({"ref": "e5", "value": "Large"}))
        .await;

    for result in [&back, &hover, &selected] {
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("- link \"Docs\""),
            "{}",
            result.content
        );
    }
    assert!(forward.is_error, "the fake history has no next page");
    assert!(
        armed.content.contains("will be accepted"),
        "{}",
        armed.content
    );
    assert!(answer.accept);
    assert!(
        listed
            .content
            .contains("Dialogs:\n- confirm \"Delete it?\" accepted\n"),
        "{}",
        listed.content
    );
    assert_eq!(
        listed.detail.as_ref().unwrap()["dialogs"][0],
        "confirm \"Delete it?\" accepted"
    );
    assert!(matches!(invalid, Err(ToolError::InvalidArgs { .. })));
    let calls = provider.calls("run-a").await;
    for method in [
        "Page.navigateToHistoryEntry",
        "Runtime.callFunctionOn",
        "Input.dispatchMouseEvent",
    ] {
        assert!(
            calls.iter().any(|call| call == method),
            "{method}: {calls:?}"
        );
    }
}

#[tokio::test]
async fn uploads_resolve_links_and_refuse_credentials_and_non_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("report.txt"), "quarterly numbers").unwrap();
    std::os::unix::fs::symlink(root.join("report.txt"), root.join("latest.txt")).unwrap();
    std::fs::write(
        root.join("id_ed25519"),
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .unwrap();
    let provider = Arc::new(FakeProvider::default());
    let upload = |paths: Vec<std::path::PathBuf>| {
        let tool = tool(&provider, "browser_upload_file");
        async move {
            tool.execute_with_context(&context("run-a"), json!({"ref": "e5", "paths": paths}))
                .await
                .unwrap()
        }
    };
    tool(&provider, "browser_snapshot")
        .execute_with_context(&context("run-a"), json!({}))
        .await
        .unwrap();

    let linked = upload(vec![root.join("latest.txt")]).await;
    let credential = upload(vec![root.join("report.txt"), root.join("id_ed25519")]).await;
    let directory = upload(vec![root.clone()]).await;
    let missing = upload(vec![root.join("absent.txt")]).await;

    assert!(!linked.is_error, "{}", linked.content);
    assert!(
        credential.is_error && credential.content.contains("private-key-block"),
        "{}",
        credential.content
    );
    assert!(
        directory.is_error && directory.content.contains("not a regular file"),
        "{}",
        directory.content
    );
    assert!(missing.is_error, "{}", missing.content);
    assert_eq!(
        provider.uploaded("run-a").await,
        [json!([root.join("report.txt")])],
        "only the resolved, credential-free upload reached the page"
    );
    let upload_tool = tool(&provider, "browser_upload_file");
    assert!(upload_tool.permissions().fs_read && upload_tool.permissions().network);
}

#[tokio::test]
async fn upload_review_names_the_receiving_site_and_workspace_relative_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/report.txt"), "quarterly numbers").unwrap();
    let elsewhere = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(elsewhere.path(), "1234").unwrap();
    let provider = Arc::new(FakeProvider::default());
    let args = json!({"ref": "e5", "paths": [root.join("docs/report.txt"), elsewhere.path()]});

    let before_page = upload_review(provider.as_ref(), "run-a", &args, Some(&root)).await;
    tool(&provider, "browser_snapshot")
        .execute_with_context(&context("run-a"), json!({}))
        .await
        .unwrap();
    let facts = upload_review(provider.as_ref(), "run-a", &args, Some(&root))
        .await
        .unwrap();

    assert!(before_page.unwrap_err().contains("no browser page is open"));
    assert_eq!(
        facts,
        json!({
            "form_origin": "https://forms.example.test",
            "form_url": "https://forms.example.test/upload?step=2",
            "page_url": "https://example.test/",
            "files": [
                {"path": "docs/report.txt", "bytes": 17},
                {"path": elsewhere.path().canonicalize().unwrap(), "bytes": 4},
            ],
        })
    );
    assert!(
        !provider
            .calls("run-a")
            .await
            .iter()
            .any(|call| call == "DOM.setFileInputFiles"),
        "reviewing never uploads"
    );
}

#[tokio::test]
async fn tab_tools_list_switch_and_close_and_outlines_report_new_tabs() {
    // Given: a run with a page whose click opened a second tab
    let provider = Arc::new(FakeProvider::default());
    let tab = |id: &str, active: bool, opener: Option<&str>| TabInfo {
        id: id.into(),
        url: format!("https://example.test/{id}"),
        title: format!("Page {id}"),
        active,
        opener: opener.map(Into::into),
    };
    *provider.tabs.lock().unwrap() = vec![tab("t1", true, None), tab("t2", false, Some("t1"))];
    provider
        .news
        .lock()
        .unwrap()
        .push("t2 opened from t1; browser_switch_tab moves to it".into());
    let run = |name: &str, args: Value| {
        let tool = tool(&provider, name);
        async move { tool.execute_with_context(&context("run-a"), args).await }
    };

    // When: the agent reads the outline, lists, switches and closes tabs
    let clicked = run("browser_snapshot", json!({})).await.unwrap();
    let listed = run("browser_tabs", json!({})).await.unwrap();
    let switched = run("browser_switch_tab", json!({"tab": "t2"}))
        .await
        .unwrap();
    let missing = run("browser_switch_tab", json!({"tab": "t9"}))
        .await
        .unwrap();
    let closed = run("browser_close_tab", json!({"tab": "t2"}))
        .await
        .unwrap();
    let invalid = run("browser_close_tab", json!({})).await.unwrap();

    // Then: the news shows once, and each tool reports the run's tabs
    assert!(
        clicked
            .content
            .contains("Tabs:\n- t2 opened from t1; browser_switch_tab moves to it\n"),
        "{}",
        clicked.content
    );
    assert_eq!(
        clicked.detail.as_ref().unwrap()["tabs"][0],
        "t2 opened from t1; browser_switch_tab moves to it"
    );
    assert_eq!(
        listed.content,
        "- t1 (active) https://example.test/t1 \"Page t1\"\n\
         - t2 https://example.test/t2 \"Page t2\", opened from t1\n"
    );
    assert_eq!(listed.detail.as_ref().unwrap()["tabs"][1]["opener"], "t1");
    assert!(!switched.is_error, "{}", switched.content);
    assert!(
        switched.content.contains("- link \"Docs\""),
        "{}",
        switched.content
    );
    assert!(!switched.content.contains("Tabs:"), "{}", switched.content);
    assert!(missing.is_error);
    assert!(missing.content.contains("no tab t9"), "{}", missing.content);
    assert_eq!(
        closed.content,
        "Closed t2.\n- t1 (active) https://example.test/t1 \"Page t1\"\n"
    );
    assert!(invalid.is_error);
    assert!(
        invalid.content.contains("missing tab"),
        "{}",
        invalid.content
    );
}
