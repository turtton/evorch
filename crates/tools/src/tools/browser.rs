//! Agent browser tools (ADR 0034): headless Chromium tabs per run, read as
//! an accessibility outline and operated by the refs in it.

use std::sync::Arc;

use browser::agent::{
    AgentError, DialogAnswer, PageAgent, PageProvider, RequestPolicy, Shown, View, WaitFor,
    WaitLimits, WaitOutcome, Waited,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::network_guard::NetworkGuard;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// Every browser tool name, including opt-in ones that are registered only
/// when configured.
pub const BROWSER_TOOL_NAMES: [&str; 19] = [
    "browser_navigate",
    "browser_navigate_back",
    "browser_navigate_forward",
    "browser_snapshot",
    "browser_click",
    "browser_click_at",
    "browser_hover",
    "browser_type",
    "browser_select_option",
    "browser_upload_file",
    "browser_press_key",
    "browser_scroll",
    "browser_wait_for",
    "browser_handle_dialog",
    "browser_tabs",
    "browser_switch_tab",
    "browser_close_tab",
    "browser_screenshot",
    "browser_close",
];

/// Which optional browser tools to register.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrowserToolOptions {
    /// `browser_click_at`: clicks at viewport coordinates.
    pub coordinate_actions: bool,
}

/// Lets the agent browser load only what `web_fetch` may fetch.
pub struct GuardPolicy(pub Arc<NetworkGuard>);

#[async_trait::async_trait]
impl RequestPolicy for GuardPolicy {
    async fn check(&self, url: &url::Url) -> Result<(), String> {
        self.0
            .check_destination(url)
            .await
            .map_err(|error| error.to_string())
    }

    async fn resolve(
        &self,
        host: &str,
        port: u16,
        tls: bool,
    ) -> Result<Vec<std::net::SocketAddr>, String> {
        if !tls {
            return Err("only HTTPS (TLS) connections are allowed".into());
        }
        self.0
            .resolve_destination(host, port)
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Navigate,
    Back,
    Forward,
    Snapshot,
    Click,
    ClickAt,
    Hover,
    Type,
    SelectOption,
    UploadFile,
    PressKey,
    Scroll,
    WaitFor,
    HandleDialog,
    Tabs,
    SwitchTab,
    CloseTab,
    Screenshot,
    Close,
}

const KINDS: [Kind; 19] = [
    Kind::Navigate,
    Kind::Back,
    Kind::Forward,
    Kind::Snapshot,
    Kind::Click,
    Kind::ClickAt,
    Kind::Hover,
    Kind::Type,
    Kind::SelectOption,
    Kind::UploadFile,
    Kind::PressKey,
    Kind::Scroll,
    Kind::WaitFor,
    Kind::HandleDialog,
    Kind::Tabs,
    Kind::SwitchTab,
    Kind::CloseTab,
    Kind::Screenshot,
    Kind::Close,
];

pub struct BrowserTool {
    kind: Kind,
    provider: Arc<dyn PageProvider>,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Navigate => "browser_navigate",
            Self::Back => "browser_navigate_back",
            Self::Forward => "browser_navigate_forward",
            Self::Snapshot => "browser_snapshot",
            Self::Click => "browser_click",
            Self::ClickAt => "browser_click_at",
            Self::Hover => "browser_hover",
            Self::Type => "browser_type",
            Self::SelectOption => "browser_select_option",
            Self::UploadFile => "browser_upload_file",
            Self::PressKey => "browser_press_key",
            Self::Scroll => "browser_scroll",
            Self::WaitFor => "browser_wait_for",
            Self::HandleDialog => "browser_handle_dialog",
            Self::Tabs => "browser_tabs",
            Self::SwitchTab => "browser_switch_tab",
            Self::CloseTab => "browser_close_tab",
            Self::Screenshot => "browser_screenshot",
            Self::Close => "browser_close",
        }
    }

    fn enabled(self, options: BrowserToolOptions) -> bool {
        self != Self::ClickAt || options.coordinate_actions
    }
}

pub fn browser_tools(
    provider: Arc<dyn PageProvider>,
    options: BrowserToolOptions,
) -> Vec<BrowserTool> {
    KINDS
        .into_iter()
        .filter(|kind| kind.enabled(options))
        .map(|kind| BrowserTool {
            kind,
            provider: Arc::clone(&provider),
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    url: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    text: Option<String>,
    #[serde(default)]
    submit: bool,
    key: Option<String>,
    direction: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    values: Option<Vec<String>>,
    paths: Option<Vec<String>>,
    accept: Option<bool>,
    prompt_text: Option<String>,
    tab: Option<String>,
    depth: Option<usize>,
    text_gone: Option<String>,
    ref_gone: Option<String>,
    #[serde(default)]
    network_idle: bool,
    #[serde(default)]
    url_changes: bool,
}

impl BrowserTool {
    async fn run(&self, key: &str, args: Args) -> Result<ToolResult, AgentError> {
        if self.kind == Kind::Close {
            let closed = self.provider.close(key).await;
            return Ok(ToolResult::success(if closed {
                "Browser page closed."
            } else {
                "No browser page was open."
            }));
        }
        match self.kind {
            Kind::Tabs => return tab_list(self.provider.as_ref(), key, String::new()).await,
            Kind::SwitchTab => {
                let page = self
                    .provider
                    .switch_tab(key, &required(args.tab, "tab")?)
                    .await?;
                return outline(self.provider.as_ref(), key, &page, View::Full).await;
            }
            Kind::CloseTab => {
                let tab = required(args.tab, "tab")?;
                self.provider.close_tab(key, &tab).await?;
                return tab_list(self.provider.as_ref(), key, format!("Closed {tab}.\n")).await;
            }
            _ => {}
        }
        let page = self.provider.page(key).await?;
        // A new document is read whole; after anything else the agent reads
        // what its action changed.
        let view = match self.kind {
            Kind::Navigate => View::Full,
            Kind::Snapshot if args.reference.is_none() && args.depth.is_none() => View::Full,
            Kind::Snapshot => View::Scoped {
                root: args.reference.clone(),
                depth: args.depth,
            },
            _ => View::Changes,
        };
        match self.kind {
            Kind::Navigate => {
                let url = required(args.url, "url")?;
                let url = upgrade(&url)?;
                page.navigate(url.as_str()).await?;
            }
            Kind::Back => page.go_back().await?,
            Kind::Forward => page.go_forward().await?,
            Kind::Click => page.click(&required(args.reference, "ref")?).await?,
            Kind::ClickAt => {
                let point = |value: Option<f64>, name: &str| {
                    value.ok_or_else(|| AgentError::Navigation(format!("missing {name}")))
                };
                page.click_at(point(args.x, "x")?, point(args.y, "y")?)
                    .await?;
            }
            Kind::Hover => page.hover(&required(args.reference, "ref")?).await?,
            Kind::Type => {
                page.type_text(
                    &required(args.reference, "ref")?,
                    &args.text.unwrap_or_default(),
                    args.submit,
                )
                .await?;
            }
            Kind::SelectOption => {
                let values = args
                    .values
                    .ok_or_else(|| AgentError::Refused("missing values".into()))?;
                page.select_option(&required(args.reference, "ref")?, &values)
                    .await?;
            }
            Kind::UploadFile => {
                let reference = required(args.reference, "ref")?;
                let paths = args
                    .paths
                    .filter(|paths| !paths.is_empty())
                    .ok_or_else(|| AgentError::Refused("missing paths".into()))?;
                let files = tokio::task::spawn_blocking(move || uploadable(paths))
                    .await
                    .map_err(|error| {
                        AgentError::Refused(format!("upload check failed: {error}"))
                    })??;
                page.upload_files(&reference, &files).await?;
            }
            Kind::PressKey => page.press(&required(args.key, "key")?).await?,
            Kind::Scroll => {
                page.scroll(
                    args.reference.as_deref(),
                    args.direction.as_deref().unwrap_or("down"),
                )
                .await?;
            }
            Kind::HandleDialog => {
                let accept = args
                    .accept
                    .ok_or_else(|| AgentError::Refused("missing accept".into()))?;
                page.answer_next_dialog(DialogAnswer {
                    accept,
                    prompt_text: args.prompt_text,
                });
                return Ok(ToolResult::success(if accept {
                    "The next confirm or prompt dialog your next browser action opens will be accepted."
                } else {
                    "The next confirm or prompt dialog your next browser action opens will be dismissed."
                }));
            }
            Kind::WaitFor => {
                let wanted = WaitFor {
                    text: args.text,
                    text_gone: args.text_gone,
                    reference: args.reference,
                    reference_gone: args.ref_gone,
                    network_idle: args.network_idle,
                    url: args.url,
                    url_changes: args.url_changes,
                };
                let limits = WaitLimits::default();
                let waited = page.wait_for(&wanted, limits).await?;
                let mut result = outline(self.provider.as_ref(), key, &page, View::Changes).await?;
                waited_header(&mut result, &waited, limits);
                return Ok(result);
            }
            Kind::Screenshot => return screenshot(&page).await,
            Kind::Snapshot | Kind::Close | Kind::Tabs | Kind::SwitchTab | Kind::CloseTab => {}
        }
        outline(self.provider.as_ref(), key, &page, view).await
    }
}

async fn outline(
    provider: &dyn PageProvider,
    key: &str,
    page: &PageAgent,
    view: View,
) -> Result<ToolResult, AgentError> {
    let snapshot = page.outline(view).await?;
    let mut text = snapshot.text;
    let blocked = page.take_blocked();
    let dialogs = page.take_dialogs();
    let tabs = provider.tab_news(key);
    for (heading, entries) in [
        ("Blocked requests", &blocked),
        ("Dialogs", &dialogs),
        ("Tabs", &tabs),
    ] {
        if !entries.is_empty() {
            text.push_str(&format!("{heading}:\n"));
            for entry in entries {
                text.push_str(&format!("- {entry}\n"));
            }
        }
    }
    Ok(ToolResult::success(text).with_detail(json!({
        "refs": snapshot.refs.len(),
        "shown": match snapshot.shown {
            Shown::Full => "full",
            Shown::Scoped => "scoped",
            Shown::Changes => "changes",
            Shown::Unchanged => "unchanged",
        },
        "blocked_requests": blocked,
        "dialogs": dialogs,
        "tabs": tabs,
    })))
}

/// Puts how the wait ended above the outline; a wait that gave up is an
/// error result, so the agent does not read the page as ready.
fn waited_header(result: &mut ToolResult, waited: &Waited, limits: WaitLimits) {
    let still = waited.unmet.join(", ");
    let header = match waited.outcome {
        WaitOutcome::Met => format!("Done waiting after {} page events.\n", waited.events),
        WaitOutcome::EventLimit => format!(
            "Gave up after {} page events, still waiting for {still}. The page kept changing; read the page below, then wait again or act.\n",
            waited.events
        ),
        WaitOutcome::TimeLimit => format!(
            "Gave up after {} seconds and {} page events, still waiting for {still}. Read the page below, then wait again or act.\n",
            limits.time.as_secs(),
            waited.events
        ),
    };
    result.content.insert_str(0, &header);
    result.is_error = waited.outcome != WaitOutcome::Met;
    if let Some(detail) = result.detail.as_mut() {
        detail["wait"] = json!({
            "met": waited.outcome == WaitOutcome::Met,
            "unmet": waited.unmet,
            "events": waited.events,
            "checks": waited.checks,
        });
    }
}

async fn tab_list(
    provider: &dyn PageProvider,
    key: &str,
    mut text: String,
) -> Result<ToolResult, AgentError> {
    let tabs = provider.tabs(key).await?;
    provider.tab_news(key);
    if tabs.is_empty() {
        text.push_str("No browser tab is open.");
    }
    for tab in &tabs {
        text.push_str(&format!(
            "- {}{} {} {:?}{}\n",
            tab.id,
            if tab.active { " (active)" } else { "" },
            tab.url,
            tab.title,
            tab.opener
                .as_ref()
                .map(|opener| format!(", opened from {opener}"))
                .unwrap_or_default(),
        ));
    }
    Ok(ToolResult::success(text).with_detail(json!({
        "tabs": tabs.iter().map(|tab| json!({
            "id": tab.id,
            "url": tab.url,
            "title": tab.title,
            "active": tab.active,
            "opener": tab.opener,
        })).collect::<Vec<_>>(),
    })))
}

async fn screenshot(page: &PageAgent) -> Result<ToolResult, AgentError> {
    let png = page.screenshot().await?;
    let checked = crate::image::check(crate::image::ImageFormat::Png, &png).map_err(|detail| {
        AgentError::Response {
            method: "Page.captureScreenshot".into(),
            detail,
        }
    })?;
    let path = crate::output::save_file(&png, "png").ok();
    let summary = format!("Screenshot of the visible viewport ({})", checked.summary());
    Ok(ToolResult::success(summary).with_image(checked.into_tool_image(path)))
}

fn required(value: Option<String>, name: &str) -> Result<String, AgentError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::Navigation(format!("missing {name}")))
}

/// Files larger than this are refused, which bounds the credential scan.
const MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// Resolves upload paths for Chromium. The executor has already placed each
/// resolved path inside the run's sandbox boundary; Chromium gets the resolved
/// path rather than following links itself. Files the secret guard, which
/// redacts tool output, would flag as credentials are refused.
fn uploadable(paths: Vec<String>) -> Result<Vec<String>, AgentError> {
    let redactor = secret_guard::SecretRedactor::from_env();
    paths
        .into_iter()
        .map(|path| {
            let refuse = |reason: String| AgentError::Refused(format!("{path}: {reason}"));
            let resolved =
                std::fs::canonicalize(&path).map_err(|error| refuse(error.to_string()))?;
            let metadata =
                std::fs::metadata(&resolved).map_err(|error| refuse(error.to_string()))?;
            if !metadata.is_file() {
                return Err(refuse("not a regular file".into()));
            }
            if metadata.len() > MAX_UPLOAD_BYTES {
                return Err(refuse(format!(
                    "{} bytes is over the {MAX_UPLOAD_BYTES}-byte upload limit",
                    metadata.len()
                )));
            }
            let content = std::fs::read(&resolved).map_err(|error| refuse(error.to_string()))?;
            if let Some(rule) = redactor.detect(&String::from_utf8_lossy(&content)) {
                return Err(refuse(format!(
                    "looks like it contains a credential ({rule}), so it is not uploaded"
                )));
            }
            resolved
                .into_os_string()
                .into_string()
                .map_err(|_| refuse("the path is not UTF-8".into()))
        })
        .collect()
}

pub(crate) async fn upload_review(
    provider: &dyn PageProvider,
    run_id: &str,
    args: &serde_json::Value,
    root: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    let reference = args
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing ref")?;
    let page = provider
        .open_page(run_id)
        .await
        .ok_or("no browser page is open for this run; take a browser_snapshot first")?;
    let target = page
        .upload_target(reference)
        .await
        .map_err(|error| error.to_string())?;
    let origin = url::Url::parse(&target.frame_url)
        .map_err(|error| format!("{}: {error}", target.frame_url))?
        .origin()
        .ascii_serialization();
    let mut files = Vec::new();
    for path in args
        .get("paths")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        let resolved = tokio::fs::canonicalize(path)
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        let bytes = tokio::fs::metadata(&resolved)
            .await
            .map_err(|error| format!("{path}: {error}"))?
            .len();
        let shown = root
            .and_then(|root| resolved.strip_prefix(root).ok())
            .unwrap_or(&resolved);
        files.push(json!({"path": shown, "bytes": bytes}));
    }
    Ok(json!({
        "form_origin": origin,
        "form_url": target.frame_url,
        "page_url": target.page_url,
        "files": files,
    }))
}

fn upgrade(url: &str) -> Result<url::Url, AgentError> {
    crate::network_guard::upgrade_to_https(url)
        .map_err(|error| AgentError::Navigation(error.to_string()))
}

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        self.kind.name()
    }

    fn description(&self) -> &str {
        match self.kind {
            Kind::Navigate => {
                "Open a URL in this run's active browser tab and return its outline. http is upgraded to https; requests to blocked addresses are refused, as in web_fetch."
            }
            Kind::Back => {
                "Go back one page in this run's browser history, then return what changed in the outline."
            }
            Kind::Forward => {
                "Go forward one page in this run's browser history, then return what changed in the outline."
            }
            Kind::Snapshot => {
                "Return the active tab as an accessibility outline. With ref, only the part under that element; with depth, at most that many levels below it (0 is the element alone). Elements carry [ref=eN] for the other browser tools. Other browser tools return only what changed in the outline since it was last read (the whole outline after browser_navigate, or when most of it changed). A ref works for as long as its element is on the page, whichever outline or change list showed it."
            }
            Kind::Click => {
                "Click the element with the given ref, then return what changed in the outline."
            }
            Kind::ClickAt => {
                "Click the point (x, y), in CSS pixels from the top-left of the visible viewport as shown by browser_screenshot, then return what changed in the outline. Prefer browser_click; use this only for targets the outline does not expose, such as a canvas."
            }
            Kind::Hover => {
                "Move the mouse over the element with the given ref, for menus and tooltips that open on hover. Returns what changed in the outline."
            }
            Kind::SelectOption => {
                "Choose options in the <select> element (combobox or listbox) with the given ref, by value or visible label: one value, or any number for a multiple select. Returns what changed in the outline."
            }
            Kind::UploadFile => {
                "Set the files of the file input with the given ref, as if the user chose them, then return what changed in the outline. paths are absolute paths to files this run's sandbox may read (the project root, this thread's $TMPDIR and read-only system, tool output and Cargo/rustup cache paths); other paths, and files that look like they contain credentials, are refused. The page may send the files to its site, under the same network rules as browser_navigate, so each call is first reviewed against the user's request."
            }
            Kind::HandleDialog => {
                "Decide how to answer the confirm or prompt dialog that your next browser action opens: accept=true presses OK (prompt_text answers a prompt), accept=false cancels. Unanswered confirm and prompt dialogs are cancelled and alerts are closed; results list each dialog."
            }
            Kind::Type => {
                "Replace the content of the field with the given ref by text (empty clears it); submit presses Enter. Returns what changed in the outline."
            }
            Kind::PressKey => {
                "Press a key in the focused element, such as Enter, Escape, ArrowDown, a or Control+a. Returns what changed in the outline."
            }
            Kind::Scroll => {
                "Scroll the page, or the element with the given ref, by most of a viewport. Returns what changed in the outline."
            }
            Kind::WaitFor => {
                "Wait until the active tab shows what you name, then return what changed in the outline. Name at least one condition; all must hold at once: text appearing or disappearing (text, text_gone; matched in the names and values of the outline), a ref from this or an earlier outline appearing or disappearing (ref, ref_gone), no request in flight (network_idle), the URL containing url, or the URL changing from what it is now (url_changes). Use it when an action's result arrives later. The page is checked again on each change, and the wait gives up after 1000 page changes or 30 seconds with an error result that still shows the page."
            }
            Kind::Tabs => {
                "List this run's browser tabs. Links and scripts that open a new tab add it to the run; the other browser tools act on the active tab."
            }
            Kind::SwitchTab => {
                "Make the tab with the given id (such as t2, from browser_tabs or a result's Tabs section) the active tab and return its outline."
            }
            Kind::CloseTab => {
                "Close the tab with the given id. If it was active, the tab that opened it, or else the newest remaining tab, becomes active; closing the last tab closes the run's page."
            }
            Kind::Screenshot => "Capture the visible part of the active tab as an image.",
            Kind::Close => "Close all of this run's browser tabs.",
        }
    }

    fn schema(&self) -> serde_json::Value {
        let reference = json!({"type": "string", "description": "A ref such as e12 from an outline of this page."});
        let (properties, required) = match self.kind {
            Kind::Navigate => (json!({"url": {"type": "string"}}), json!(["url"])),
            Kind::Click | Kind::Hover => (json!({"ref": reference}), json!(["ref"])),
            Kind::SelectOption => (
                json!({"ref": reference, "values": {"type": "array", "items": {"type": "string"}, "description": "Option values or visible labels."}}),
                json!(["ref", "values"]),
            ),
            Kind::UploadFile => (
                json!({"ref": reference, "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Absolute file paths inside this run's sandbox boundary."}}),
                json!(["ref", "paths"]),
            ),
            Kind::HandleDialog => (
                json!({"accept": {"type": "boolean"}, "prompt_text": {"type": "string"}}),
                json!(["accept"]),
            ),
            Kind::ClickAt => (
                json!({"x": {"type": "number", "minimum": 0}, "y": {"type": "number", "minimum": 0}}),
                json!(["x", "y"]),
            ),
            Kind::Type => (
                json!({"ref": reference, "text": {"type": "string"}, "submit": {"type": "boolean", "default": false}}),
                json!(["ref", "text"]),
            ),
            Kind::PressKey => (json!({"key": {"type": "string"}}), json!(["key"])),
            Kind::WaitFor => {
                let earlier = json!({"type": "string", "description": "A ref such as e12 from this or an earlier outline."});
                (
                    json!({
                        "text": {"type": "string"},
                        "text_gone": {"type": "string"},
                        "ref": earlier,
                        "ref_gone": earlier,
                        "network_idle": {"type": "boolean", "default": false},
                        "url": {"type": "string", "description": "Text the page URL should contain."},
                        "url_changes": {"type": "boolean", "default": false},
                    }),
                    json!([]),
                )
            }
            Kind::SwitchTab | Kind::CloseTab => (
                json!({"tab": {"type": "string", "description": "A tab id such as t2."}}),
                json!(["tab"]),
            ),
            Kind::Scroll => (
                json!({"direction": {"type": "string", "enum": ["up", "down", "left", "right"], "default": "down"}, "ref": reference}),
                json!([]),
            ),
            Kind::Snapshot => (
                json!({"ref": reference, "depth": {"type": "integer", "minimum": 0, "description": "Levels below the element (or the page) to show."}}),
                json!([]),
            ),
            Kind::Back | Kind::Forward | Kind::Tabs | Kind::Screenshot | Kind::Close => {
                (json!({}), json!([]))
            }
        };
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: self.kind == Kind::UploadFile,
            ..Permissions::network()
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.execute_with_context(
            &ToolExecutionContext {
                run_id: "default".into(),
                thread_id: None,
                call_id: None,
            },
            args,
        )
        .await
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let args: Args = serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
            detail: format!("{}: {error}", self.name()),
        })?;
        Ok(self
            .run(&ctx.run_id, args)
            .await
            .unwrap_or_else(|error| ToolResult::error(error.to_string())))
    }

    fn end_run(&self, run_id: &str) {
        if self.kind == Kind::Close {
            self.provider.release(run_id);
        }
    }
}

#[cfg(test)]
mod tests;
