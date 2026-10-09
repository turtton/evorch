//! Conversation artifacts (ADR 0029): agents capture images or HTML mocks with
//! `render_artifact`, and only the conversation owner shows them with `present`.
//!
//! Captures are immutable content-addressed copies, so later edits to the
//! working file never change what the user was shown.
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::agent_categories::CategoryId;
use event_bus::{ArtifactPresentation, Event, PresentedArtifact, ToolEvent};
use providers::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{AgentRuntime, Role, RunPurpose, agent_loop::LoopState, meta::DispatchResult};

pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_HTML_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TITLE: usize = 200;
const MAX_CAPTION: usize = 1000;
const MAX_PRESENTED: usize = 8;

/// The formats a conversation card can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    Png,
    Jpeg,
    Gif,
    Webp,
    Html,
}

impl ArtifactKind {
    fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match extension.as_str() {
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            "gif" => Self::Gif,
            "webp" => Self::Webp,
            "html" | "htm" => Self::Html,
            _ => return None,
        })
    }

    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
            Self::Html => "text/html",
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Html => "html",
        }
    }

    const fn max_bytes(self) -> u64 {
        match self {
            Self::Html => MAX_HTML_BYTES,
            _ => MAX_IMAGE_BYTES,
        }
    }

    /// Reject content that does not match its extension, so the GUI never
    /// decodes a mislabeled file and HTML is never mistaken for an image.
    fn matches(self, bytes: &[u8]) -> bool {
        match self {
            Self::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            Self::Jpeg => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
            Self::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
            Self::Webp => bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP",
            Self::Html => !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok(),
        }
    }
}

/// Durable metadata of one capture. Ownership is checked against it on `present`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub sha256: String,
    pub media_type: String,
    pub extension: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub source_path: String,
    pub byte_len: u64,
    pub run_id: String,
    pub root_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// Captures live outside every workspace: `<root>/blobs/<sha256>.<ext>` and
/// `<root>/meta/<artifact_id>.json`.
#[derive(Debug)]
pub struct ArtifactStore {
    root: PathBuf,
}

/// The capture inputs that do not come from the file itself.
pub struct CaptureRequest<'a> {
    pub title: &'a str,
    pub caption: Option<&'a str>,
    pub run_id: String,
    pub root_run_id: String,
    pub thread_id: Option<String>,
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("meta"))?;
        Ok(Self { root })
    }

    pub fn blob_path(&self, record: &ArtifactRecord) -> PathBuf {
        self.root
            .join("blobs")
            .join(format!("{}.{}", record.sha256, record.extension))
    }

    /// Validate `path` inside `workspace` and store its current content.
    pub fn capture(
        &self,
        workspace: &Path,
        path: &Path,
        request: CaptureRequest<'_>,
    ) -> Result<ArtifactRecord, String> {
        let (source, kind, bytes) = read_workspace_file(workspace, path)?;
        if kind == ArtifactKind::Html
            && let Some(rule) = secret_guard::SecretRedactor::from_env()
                .detect(std::str::from_utf8(&bytes).unwrap_or_default())
        {
            return Err(format!(
                "the HTML contains a credential ({rule:?}); remove it before capturing"
            ));
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let record = ArtifactRecord {
            artifact_id: random_id("artifact")?,
            sha256,
            media_type: kind.media_type().into(),
            extension: kind.extension().into(),
            title: request.title.into(),
            caption: request.caption.map(str::to_owned),
            source_path: source.to_string_lossy().into_owned(),
            byte_len: bytes.len() as u64,
            run_id: request.run_id,
            root_run_id: request.root_run_id,
            thread_id: request.thread_id,
        };
        let blob = self.blob_path(&record);
        if !blob.is_file() {
            write_atomically(&blob, &bytes)?;
        }
        let meta = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
        write_atomically(&self.meta_path(&record.artifact_id)?, &meta)?;
        Ok(record)
    }

    pub fn load(&self, artifact_id: &str) -> Result<ArtifactRecord, String> {
        let path = self.meta_path(artifact_id)?;
        let bytes = std::fs::read(&path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => format!("unknown artifact: {artifact_id}"),
            _ => format!("cannot read artifact {artifact_id}: {error}"),
        })?;
        let record: ArtifactRecord = serde_json::from_slice(&bytes)
            .map_err(|error| format!("corrupt artifact {artifact_id}: {error}"))?;
        if record.artifact_id != artifact_id {
            return Err(format!("corrupt artifact {artifact_id}: identity mismatch"));
        }
        Ok(record)
    }

    fn meta_path(&self, artifact_id: &str) -> Result<PathBuf, String> {
        let valid = artifact_id
            .strip_prefix("artifact-")
            .is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
        if !valid {
            return Err(format!("invalid artifact id: {artifact_id}"));
        }
        Ok(self.root.join("meta").join(format!("{artifact_id}.json")))
    }
}

fn read_workspace_file(
    workspace: &Path,
    path: &Path,
) -> Result<(PathBuf, ArtifactKind, Vec<u8>), String> {
    let workspace = workspace
        .canonicalize()
        .map_err(|error| format!("workspace is unavailable: {error}"))?;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    // Resolve links first: only the real file must be inside the workspace.
    let source = joined
        .canonicalize()
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    if !source.starts_with(&workspace) {
        return Err(format!(
            "{} is outside this run's workspace",
            path.display()
        ));
    }
    let kind = ArtifactKind::from_extension(&source).ok_or_else(|| {
        format!(
            "{} is not a supported artifact; use .png, .jpg, .gif, .webp or .html",
            path.display()
        )
    })?;
    let file = std::fs::File::open(&source)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let limit = kind.max_bytes();
    let mut bytes = Vec::new();
    // Bound the read itself: the file may grow after the metadata check.
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "{} exceeds the {} MiB limit for {}",
            path.display(),
            limit / 1024 / 1024,
            kind.media_type()
        ));
    }
    if bytes.is_empty() || !kind.matches(&bytes) {
        return Err(format!(
            "{} does not contain valid {} data",
            path.display(),
            kind.media_type()
        ));
    }
    Ok((source, kind, bytes))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let directory = path.parent().ok_or("artifact path has no directory")?;
    let temporary = directory.join(format!(".{}.tmp", random_id("partial")?));
    std::fs::write(&temporary, bytes)
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("cannot store artifact: {error}")
        })
}

fn random_id(prefix: &str) -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(format!("{prefix}-{:032x}", u128::from_be_bytes(bytes)))
}

pub fn reference(artifact_id: &str, title: &str) -> String {
    format!("[artifact {artifact_id}: {title}]")
}

fn bounded_text(value: &str, max: usize, field: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(format!("{field} must contain 1..={max} characters"));
    }
    Ok(())
}

/// A General Worker in the visual category, or the conversation's own root
/// Worker. Category and root come from the runtime, never from model input.
pub(crate) fn may_render(role: Role, config: &crate::RunConfig, is_root: bool) -> bool {
    let category = config.category.as_deref();
    role == Role::Worker
        && config.purpose == RunPurpose::General
        && (category == Some(CategoryId::Visual.as_str())
            || (config.conversation
                && is_root
                && category == Some(CategoryId::Conversation.as_str())))
}

pub(crate) fn render_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "Capture an image or HTML mock from your workspace so the conversation owner can show it to the user. Supported: .png, .jpg, .gif, .webp (up to 8 MiB) and self-contained .html (up to 2 MiB; inline all CSS, scripts and images because external files are not captured). The current file content is stored as an immutable copy, so capture again after each revision. This does not show anything to the user: pass the returned artifact_id to present if you own the conversation, otherwise include it in your result for your parent.".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string","description":"File inside your workspace; relative paths resolve from the workspace root."},"title":{"type":"string","minLength":1,"maxLength":MAX_TITLE},"caption":{"type":"string","maxLength":MAX_CAPTION}},"required":["path","title"],"additionalProperties":false}),
    }
}

pub(crate) fn present_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: "Show captured artifacts to the user as cards in this conversation, for example UI mock variants to compare. Accepts artifact_id values from render_artifact, including those your delegated agents returned. All listed artifacts must belong to this conversation; nothing is shown if any is invalid. The user sees images directly and opens HTML mocks in their browser.".into(),
        input_schema: json!({"type":"object","properties":{"artifact_ids":{"type":"array","minItems":1,"maxItems":MAX_PRESENTED,"uniqueItems":true,"items":{"type":"string"}},"title":{"type":"string","minLength":1,"maxLength":MAX_TITLE},"caption":{"type":"string","maxLength":MAX_CAPTION}},"required":["artifact_ids"],"additionalProperties":false}),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenderArgs {
    path: String,
    title: String,
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentArgs {
    artifact_ids: Vec<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    caption: Option<String>,
}

fn store(runtime: &AgentRuntime) -> Result<Arc<ArtifactStore>, String> {
    runtime
        .artifact_store()
        .ok_or_else(|| "artifact storage is not configured".into())
}

fn finish(result: Result<Value, String>) -> DispatchResult {
    match result {
        Ok(value) => crate::meta::success(value.to_string()),
        Err(reason) => crate::meta::error(reason),
    }
}

pub(crate) fn render(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    finish((|| {
        if !may_render(
            state.run_role(),
            state.run_config(),
            state.task.parent.is_none(),
        ) {
            return Err(
                "only a visual Worker or the conversation's root Worker may capture artifacts"
                    .into(),
            );
        }
        let args: RenderArgs = crate::meta::parse(input)?;
        bounded_text(&args.title, MAX_TITLE, "title")?;
        if let Some(caption) = &args.caption {
            bounded_text(caption, MAX_CAPTION, "caption")?;
        }
        let store = store(runtime)?;
        let caller = state.caller_run_id();
        runtime.validate_artifact_mutation(caller)?;
        let workspace = state
            .shared
            .executor
            .default_cwd()
            .ok_or("this run has no workspace to capture from")?;
        let root = runtime.run_root(caller)?;
        let record = store.capture(
            &workspace,
            Path::new(&args.path),
            CaptureRequest {
                title: &args.title,
                caption: args.caption.as_deref(),
                run_id: caller.to_string(),
                root_run_id: root.to_string(),
                thread_id: runtime.goal_thread(root),
            },
        )?;
        Ok(json!({
            "artifact_id": record.artifact_id,
            "title": record.title,
            "media_type": record.media_type,
            "bytes": record.byte_len,
            "reference": reference(&record.artifact_id, &record.title),
        }))
    })())
}

pub(crate) fn present(state: &LoopState, runtime: &AgentRuntime, input: Value) -> DispatchResult {
    finish((|| {
        if !crate::thread_todos::conversation_root(state) {
            return Err(
                "only the conversation's root Worker or Orchestrator may present artifacts".into(),
            );
        }
        let args: PresentArgs = crate::meta::parse(input)?;
        if args.artifact_ids.is_empty() || args.artifact_ids.len() > MAX_PRESENTED {
            return Err(format!(
                "artifact_ids must list 1..={MAX_PRESENTED} artifacts"
            ));
        }
        let mut unique = std::collections::BTreeSet::new();
        if !args.artifact_ids.iter().all(|id| unique.insert(id)) {
            return Err("artifact_ids must not repeat an artifact".into());
        }
        if let Some(title) = &args.title {
            bounded_text(title, MAX_TITLE, "title")?;
        }
        if let Some(caption) = &args.caption {
            bounded_text(caption, MAX_CAPTION, "caption")?;
        }
        let store = store(runtime)?;
        let caller = state.caller_run_id();
        let thread = runtime.goal_thread(caller);
        let mut artifacts = Vec::with_capacity(args.artifact_ids.len());
        // Validate every artifact before showing any of them.
        for id in &args.artifact_ids {
            let record = store.load(id)?;
            let same_root = record.root_run_id == caller.to_string();
            let same_thread = thread.is_some() && record.thread_id == thread;
            if !same_root && !same_thread {
                return Err(format!(
                    "artifact {id} does not belong to this conversation"
                ));
            }
            let path = store.blob_path(&record);
            if !path.is_file() {
                return Err(format!("artifact {id} is no longer stored"));
            }
            artifacts.push(PresentedArtifact {
                artifact_id: record.artifact_id,
                title: record.title,
                caption: record.caption,
                media_type: record.media_type,
                path: path.to_string_lossy().into_owned(),
                byte_len: record.byte_len,
                sha256: record.sha256,
            });
        }
        let presentation = ArtifactPresentation {
            presentation_id: random_id("presentation")?,
            title: args.title,
            caption: args.caption,
            artifacts,
        };
        // Match todo_write: keep the ownership generation fixed until the event is emitted.
        let _guard = state
            .run_config()
            .ownership
            .as_ref()
            .map(|permit| {
                permit
                    .validate_mutation()
                    .map_err(|error| error.to_string())?;
                permit.mutation_guard().map_err(|error| error.to_string())
            })
            .transpose()?;
        let references = presentation
            .artifacts
            .iter()
            .map(|artifact| reference(&artifact.artifact_id, &artifact.title))
            .collect::<Vec<_>>();
        runtime
            .shared
            .bus
            .emit(Event::new(ToolEvent::ArtifactsPresented {
                run_id: caller.to_string(),
                presentation,
            }));
        Ok(json!({
            "presented": references,
            "note": "The user now sees these artifacts in the conversation.",
        }))
    })())
}

#[cfg(test)]
mod tests;
