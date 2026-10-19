//! `inspect_artifact` (ADR 0034): an agent opens an HTML artifact of its
//! conversation in an offline page and operates it by accessibility refs.

use std::sync::Arc;

use browser::agent::{AgentError, PageAgent};
use providers::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};
use tools::ToolResult;

use crate::meta::{DispatchResult, Terminal};
use crate::{AgentRuntime, agent_loop::LoopState};

pub const INSPECT_ARTIFACT: &str = "inspect_artifact";

/// Offline pages for HTML artifacts, one per key (a run). The GUI provides
/// them on CEF; without a provider the tool is not offered.
#[async_trait::async_trait]
pub trait ArtifactPages: Send + Sync {
    /// Loads `html` as `key`'s page, replacing the page `key` had.
    async fn open(&self, key: &str, html: Vec<u8>) -> Result<Arc<dyn ArtifactPage>, String>;

    fn page(&self, key: &str) -> Option<Arc<dyn ArtifactPage>>;

    /// Closes `key`'s page without waiting for it; returns whether one was open.
    fn close(&self, key: &str) -> bool;
}

pub trait ArtifactPage: Send + Sync {
    fn agent(&self) -> &PageAgent;

    /// Requests the offline policy refused since the last call.
    fn take_blocked(&self) -> Vec<String>;

    /// Dialogs the page opened since the last call. They are answered without
    /// anyone seeing them: alerts closed, confirm and prompt dismissed.
    fn take_dialogs(&self) -> Vec<String>;
}

pub(crate) fn spec(name: &str) -> ToolSpec {
    let reference =
        json!({"type": "string", "description": "A ref such as e12 from the latest outline."});
    ToolSpec {
        name: name.into(),
        description: "Check an HTML artifact of this conversation as a user would, in an offline browser page. open loads an HTML artifact_id from render_artifact and returns an accessibility outline whose elements carry [ref=eN]. click, hover, select_option, type, press_key and scroll act on refs from the latest outline and return the updated outline (hover opens menus and tooltips; select_option chooses <select> options by value or visible label); snapshot re-reads it; screenshot returns an image of the 1024x768 viewport; close discards the page. The page has no network access and cannot navigate away from the artifact, so anything not inlined fails to load and is listed as blocked. Nobody sees its dialogs: alert is closed, confirm and prompt are dismissed (false and null), and each is listed. Each run has one page; open replaces it.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["open", "snapshot", "click", "hover", "select_option", "type", "press_key", "scroll", "screenshot", "close"]},
                "artifact_id": {"type": "string", "description": "For open."},
                "ref": reference,
                "text": {"type": "string", "description": "For type: replaces the field's content; empty clears it."},
                "values": {"type": "array", "items": {"type": "string"}, "description": "For select_option: option values or visible labels; several for a multiple select."},
                "submit": {"type": "boolean", "description": "For type: press Enter afterwards."},
                "key": {"type": "string", "description": "For press_key, such as Enter, Escape, ArrowDown or Control+a."},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "For scroll; defaults to down."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Open,
    Snapshot,
    Click,
    Hover,
    SelectOption,
    Type,
    PressKey,
    Scroll,
    Screenshot,
    Close,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    artifact_id: Option<String>,
    #[serde(rename = "ref")]
    reference: Option<String>,
    text: Option<String>,
    values: Option<Vec<String>>,
    #[serde(default)]
    submit: bool,
    key: Option<String>,
    direction: Option<String>,
}

fn may_inspect(state: &LoopState) -> bool {
    crate::thread_todos::conversation_root(state)
        || super::may_render(
            state.run_role(),
            state.run_config(),
            state.task.parent.is_none(),
        )
}

pub(crate) async fn inspect(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    match run(state, runtime, input).await {
        Ok(result) => DispatchResult {
            result,
            terminal: Terminal::Continue,
        },
        Err(reason) => crate::meta::error(reason),
    }
}

async fn run(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> Result<ToolResult, String> {
    if !may_inspect(state) {
        return Err(
            "only a visual Worker or the conversation's root Worker or Orchestrator may inspect artifacts"
                .into(),
        );
    }
    let pages = runtime
        .artifact_pages()
        .ok_or("artifact inspection needs the GUI built with CEF")?;
    let args: Args = crate::meta::parse(input)?;
    let key = state.caller_run_id().to_string();
    let page = match args.action {
        Action::Open => {
            let id = required(args.artifact_id, "artifact_id")?;
            let html = artifact_html(state, runtime, &id)?;
            pages.open(&key, html).await?
        }
        Action::Close => {
            return Ok(ToolResult::success(if pages.close(&key) {
                "Artifact page closed."
            } else {
                "No artifact page was open."
            }));
        }
        _ => pages
            .page(&key)
            .ok_or("no artifact page is open; open an artifact_id first")?,
    };
    let agent = page.agent();
    match args.action {
        Action::Click => agent
            .click(&required(args.reference, "ref")?)
            .await
            .map_err(describe)?,
        Action::Hover => agent
            .hover(&required(args.reference, "ref")?)
            .await
            .map_err(describe)?,
        Action::SelectOption => agent
            .select_option(
                &required(args.reference, "ref")?,
                &args
                    .values
                    .filter(|values| !values.is_empty())
                    .ok_or("missing values")?,
            )
            .await
            .map_err(describe)?,
        Action::Type => agent
            .type_text(
                &required(args.reference, "ref")?,
                &args.text.unwrap_or_default(),
                args.submit,
            )
            .await
            .map_err(describe)?,
        Action::PressKey => agent
            .press(&required(args.key, "key")?)
            .await
            .map_err(describe)?,
        Action::Scroll => agent
            .scroll(
                args.reference.as_deref(),
                args.direction.as_deref().unwrap_or("down"),
            )
            .await
            .map_err(describe)?,
        Action::Screenshot => return screenshot(agent).await,
        Action::Open | Action::Snapshot | Action::Close => {}
    }
    outline(page.as_ref()).await
}

/// The artifact's HTML, if it belongs to the caller's conversation.
fn artifact_html(state: &LoopState, runtime: &AgentRuntime, id: &str) -> Result<Vec<u8>, String> {
    let store = super::store(runtime)?;
    let record = store.load(id)?;
    let root = runtime.run_root(state.caller_run_id())?;
    let thread = runtime.goal_thread(root);
    let same_root = record.root_run_id == root.to_string();
    let same_thread = thread.is_some() && record.thread_id == thread;
    if !same_root && !same_thread {
        return Err(format!(
            "artifact {id} does not belong to this conversation"
        ));
    }
    if record.media_type != super::ArtifactKind::Html.media_type() {
        return Err(format!(
            "artifact {id} is {}; only HTML artifacts can be inspected",
            record.media_type
        ));
    }
    std::fs::read(store.blob_path(&record))
        .map_err(|error| format!("artifact {id} is no longer stored: {error}"))
}

async fn outline(page: &dyn ArtifactPage) -> Result<ToolResult, String> {
    let snapshot = page.agent().snapshot().await.map_err(describe)?;
    let mut text = snapshot.text;
    let blocked = page.take_blocked();
    if !blocked.is_empty() {
        text.push_str("Blocked requests:\n");
        for entry in &blocked {
            text.push_str(&format!("- {entry}\n"));
        }
    }
    let dialogs = page.take_dialogs();
    if !dialogs.is_empty() {
        text.push_str("Dialogs:\n");
        for entry in &dialogs {
            text.push_str(&format!("- {entry}\n"));
        }
    }
    Ok(ToolResult::success(text).with_detail(json!({
        "refs": snapshot.refs.len(),
        "blocked_requests": blocked,
        "dialogs": dialogs,
    })))
}

async fn screenshot(agent: &PageAgent) -> Result<ToolResult, String> {
    let png = agent.screenshot().await.map_err(describe)?;
    let checked = tools::image::check(tools::image::ImageFormat::Png, &png)?;
    let path = tools::output::save_file(&png, "png").ok();
    let summary = format!(
        "Screenshot of the artifact viewport ({})",
        checked.summary()
    );
    Ok(ToolResult::success(summary).with_image(checked.into_tool_image(path)))
}

fn required(value: Option<String>, name: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

fn describe(error: AgentError) -> String {
    match error {
        AgentError::UnknownRef(reference) => format!(
            "unknown ref {reference}; take a new snapshot and use a ref from the latest outline"
        ),
        other => other.to_string(),
    }
}
