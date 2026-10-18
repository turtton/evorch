//! Agent browser tools (ADR 0034): a headless Chromium page per run, read as
//! an accessibility outline and operated by the refs in it.

use std::sync::Arc;

use browser::agent::{AgentError, PageAgent, PageProvider, RequestPolicy};
use serde::Deserialize;
use serde_json::json;

use crate::error::ToolError;
use crate::executor::ToolExecutionContext;
use crate::network_guard::NetworkGuard;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

pub const BROWSER_TOOL_NAMES: [&str; 8] = [
    "browser_navigate",
    "browser_snapshot",
    "browser_click",
    "browser_type",
    "browser_press_key",
    "browser_scroll",
    "browser_screenshot",
    "browser_close",
];

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Navigate,
    Snapshot,
    Click,
    Type,
    PressKey,
    Scroll,
    Screenshot,
    Close,
}

const KINDS: [Kind; 8] = [
    Kind::Navigate,
    Kind::Snapshot,
    Kind::Click,
    Kind::Type,
    Kind::PressKey,
    Kind::Scroll,
    Kind::Screenshot,
    Kind::Close,
];

pub struct BrowserTool {
    kind: Kind,
    provider: Arc<dyn PageProvider>,
}

pub fn browser_tools(provider: Arc<dyn PageProvider>) -> Vec<BrowserTool> {
    KINDS
        .into_iter()
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
        let page = self.provider.page(key).await?;
        match self.kind {
            Kind::Navigate => {
                let url = required(args.url, "url")?;
                let url = upgrade(&url)?;
                page.navigate(url.as_str()).await?;
            }
            Kind::Click => page.click(&required(args.reference, "ref")?).await?,
            Kind::Type => {
                page.type_text(
                    &required(args.reference, "ref")?,
                    &args.text.unwrap_or_default(),
                    args.submit,
                )
                .await?;
            }
            Kind::PressKey => page.press(&required(args.key, "key")?).await?,
            Kind::Scroll => {
                page.scroll(
                    args.reference.as_deref(),
                    args.direction.as_deref().unwrap_or("down"),
                )
                .await?;
            }
            Kind::Screenshot => return screenshot(&page).await,
            Kind::Snapshot | Kind::Close => {}
        }
        outline(&page).await
    }
}

async fn outline(page: &PageAgent) -> Result<ToolResult, AgentError> {
    let snapshot = page.snapshot().await?;
    let mut text = snapshot.text;
    let blocked = page.take_blocked();
    if !blocked.is_empty() {
        text.push_str("Blocked requests:\n");
        for entry in &blocked {
            text.push_str(&format!("- {entry}\n"));
        }
    }
    Ok(ToolResult::success(text).with_detail(json!({
        "refs": snapshot.refs.len(),
        "blocked_requests": blocked,
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

fn upgrade(url: &str) -> Result<url::Url, AgentError> {
    crate::network_guard::upgrade_to_https(url)
        .map_err(|error| AgentError::Navigation(error.to_string()))
}

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        BROWSER_TOOL_NAMES[KINDS
            .iter()
            .position(|kind| *kind == self.kind)
            .unwrap_or(0)]
    }

    fn description(&self) -> &str {
        match self.kind {
            Kind::Navigate => {
                "Open a URL in this run's headless browser page and return its outline. http is upgraded to https; requests to blocked addresses are refused, as in web_fetch."
            }
            Kind::Snapshot => {
                "Return the current page as an accessibility outline. Elements carry [ref=eN] for the other browser tools; only refs from the latest outline are accepted."
            }
            Kind::Click => "Click the element with the given ref, then return the updated outline.",
            Kind::Type => {
                "Replace the content of the field with the given ref by text (empty clears it); submit presses Enter. Returns the updated outline."
            }
            Kind::PressKey => {
                "Press a key in the focused element, such as Enter, Escape, ArrowDown, a or Control+a. Returns the updated outline."
            }
            Kind::Scroll => {
                "Scroll the page, or the element with the given ref, by most of a viewport. Returns the updated outline."
            }
            Kind::Screenshot => "Capture the visible part of the page as an image.",
            Kind::Close => "Close this run's browser page.",
        }
    }

    fn schema(&self) -> serde_json::Value {
        let reference =
            json!({"type": "string", "description": "A ref such as e12 from the latest outline."});
        let (properties, required) = match self.kind {
            Kind::Navigate => (json!({"url": {"type": "string"}}), json!(["url"])),
            Kind::Click => (json!({"ref": reference}), json!(["ref"])),
            Kind::Type => (
                json!({"ref": reference, "text": {"type": "string"}, "submit": {"type": "boolean", "default": false}}),
                json!(["ref", "text"]),
            ),
            Kind::PressKey => (json!({"key": {"type": "string"}}), json!(["key"])),
            Kind::Scroll => (
                json!({"direction": {"type": "string", "enum": ["up", "down", "left", "right"], "default": "down"}, "ref": reference}),
                json!([]),
            ),
            Kind::Snapshot | Kind::Screenshot | Kind::Close => (json!({}), json!([])),
        };
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::network()
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
