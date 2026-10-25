//! Runtime-owned image material generation (ADR 0038).
mod processing;
#[cfg(test)]
mod tests;

use crate::{
    AgentInvocationContext, AgentRuntime,
    agent_loop::LoopState,
    meta::{DispatchResult, Terminal},
};
use providers::{ImageGenerationRequest, ImageGenerationResponse, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct GeneratedImage {
    pub response: ImageGenerationResponse,
    pub profile: String,
    pub model: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    prompt: String,
    output_path: PathBuf,
    #[serde(default)]
    reference_image_paths: Vec<PathBuf>,
    #[serde(default)]
    transparent_background: bool,
    variant: Option<Variant>,
    size: Option<String>,
    quality: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Variant {
    max_side: u32,
    format: VariantFormat,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VariantFormat {
    Jpeg,
    Png,
}

impl Args {
    fn validate(&self) -> Result<(), String> {
        if self.prompt.trim().is_empty() {
            return Err("prompt is required".into());
        }
        if self.output_path.extension().and_then(|v| v.to_str()) != Some("png") {
            return Err("output_path must end in .png".into());
        }
        if self.reference_image_paths.len() > 5 {
            return Err("at most 5 reference images are allowed".into());
        }
        if let Some(variant) = &self.variant {
            if !(64..=2048).contains(&variant.max_side) {
                return Err("variant.max_side must be 64..=2048".into());
            }
            if self.transparent_background && matches!(variant.format, VariantFormat::Jpeg) {
                return Err("jpeg variant cannot have a transparent background".into());
            }
        }
        Ok(())
    }

    fn variant_path(&self) -> Option<PathBuf> {
        let variant = self.variant.as_ref()?;
        let mut stem = self.output_path.file_stem()?.to_os_string();
        let extension = match variant.format {
            VariantFormat::Jpeg => "jpg",
            VariantFormat::Png => "png",
        };
        stem.push(format!(".{}.{extension}", variant.max_side));
        Some(self.output_path.with_file_name(stem))
    }
}

pub(crate) fn spec(name: &str) -> ToolSpec {
    ToolSpec { name: name.into(),
        description: "Generate or edit one PNG material image via OpenAI (Codex login or OpenAI API). Reference images are sent to OpenAI without a separate approval. Images consume substantial usage quota. Choose workspace or this thread's $TMPDIR; existing files are overwritten, parents must exist. Up to 5 png/jpeg/gif/webp references, each <=8 MiB, are resized to <=2048px before sending. No masks. Optional variant: max_side 64..2048, jpeg (quality 85) or png; jpeg with transparent_background is rejected. size and quality may be ignored by Codex. A <=1024px preview is returned. Decide whether to adopt the image or use it only as reference. Do not substitute generated images for UI mocks: build the UI in HTML, reference material variants by relative path, then render_artifact.".into(),
        input_schema: json!({"type":"object", "properties": {
            "prompt":{"type":"string","minLength":1},
            "output_path":{"type":"string","description":"Required .png path in workspace or this thread's $TMPDIR"},
            "reference_image_paths":{"type":"array","maxItems":5,"items":{"type":"string"}},
            "transparent_background":{"type":"boolean"},
            "variant":{"type":"object","properties":{"max_side":{"type":"integer","minimum":64,"maximum":2048},"format":{"type":"string","enum":["jpeg","png"]}},"required":["max_side","format"],"additionalProperties":false},
            "size":{"type":"string"}, "quality":{"type":"string"}
        },"required":["prompt","output_path"],"additionalProperties":false}) }
}

fn allowed(state: &LoopState) -> bool {
    crate::artifacts::may_render(
        state.run_role(),
        state.run_config(),
        state.task.parent.is_none(),
    ) && state.image_model.image_generation_enabled()
}

pub(crate) async fn dispatch(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> DispatchResult {
    let result = execute(state, runtime, input).await;
    DispatchResult {
        result: result.unwrap_or_else(tools::ToolResult::error),
        terminal: Terminal::Continue,
    }
}

async fn execute(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: Value,
) -> Result<tools::ToolResult, String> {
    if !allowed(state) {
        return Err("image generation requires a visual Worker or conversation root Worker and a configured image chain".into());
    }
    let args: Args = crate::meta::parse(input)?;
    args.validate()?;
    runtime.validate_artifact_mutation(state.caller_run_id())?;
    let workspace = state
        .shared
        .executor
        .default_cwd()
        .ok_or("this run has no workspace")?;
    let roots = std::sync::Arc::new(crate::artifacts::SourceRoots::new(
        &workspace,
        state.scratch_dir(),
    )?);
    let (path, _) = roots.destination(&args.output_path)?;
    let variant_path = args.variant_path();
    if let Some(path) = &variant_path {
        roots.destination(path)?;
    }
    let preparation = args.clone();
    let read_roots = std::sync::Arc::clone(&roots);
    let prepare = tokio::task::spawn_blocking(move || {
        processing::references(&read_roots, &preparation.reference_image_paths)
    });
    let references = tokio::select! {
        biased;
        _ = state.channels.cancel_rx.changed() => return Err("image generation interrupted; no files saved".into()),
        result = prepare => result.map_err(|e| e.to_string())??,
    };
    if state.interrupted().is_some() {
        return Err("image generation interrupted; no files saved".into());
    }
    let invocation = AgentInvocationContext {
        run_id: state.caller_run_id().to_string(),
        turn: Some(state.turn.clone()),
        ..Default::default()
    };
    let request = ImageGenerationRequest {
        prompt: args.prompt.clone(),
        reference_images: references,
        background: args
            .transparent_background
            .then_some(providers::ImageBackground::Transparent),
        size: args.size.clone(),
        quality: args.quality.clone(),
        ..Default::default()
    };
    let generated = tokio::select! {
        biased;
        _ = state.channels.cancel_rx.changed() => return Err("image generation interrupted; no files saved".into()),
        result = state.image_model.generate_image(&invocation, request) => result?,
    };
    let png = generated.response.png;
    let original = png.clone();
    let variant = args.variant.clone();
    let processed = tokio::select! {
        biased;
        _ = state.channels.cancel_rx.changed() => return Err("image generation interrupted; no files saved".into()),
        result = tokio::task::spawn_blocking(move || processing::outputs(&png, variant.as_ref())) => result.map_err(|e| e.to_string())??,
    };
    if state.interrupted().is_some() {
        return Err("image generation interrupted; no files saved".into());
    }
    // Revalidate ownership after the long provider request and before any mutation.
    runtime.validate_artifact_mutation(state.caller_run_id())?;
    let (path, directory) = roots.destination(&path)?;
    let variant_destination = variant_path
        .as_deref()
        .map(|p| roots.destination(p))
        .transpose()?;
    let _ownership = state
        .run_config()
        .ownership
        .as_ref()
        .map(|permit| {
            permit.validate_mutation().map_err(|e| e.to_string())?;
            permit.mutation_guard().map_err(|e| e.to_string())
        })
        .transpose()?;
    if state.interrupted().is_some() {
        return Err("image generation interrupted; no files saved".into());
    }
    if let Some(((variant_path, directory), bytes)) =
        variant_destination.as_ref().zip(processed.variant.as_ref())
    {
        save(directory, variant_path, bytes)?;
    }
    save(&directory, &path, &original)?;
    let note = if args.reference_image_paths.is_empty() {
        "No reference images sent. Generation was sent to OpenAI."
    } else {
        "reference images were sent to OpenAI"
    };
    let result = json!({"path":path,"variant_path":variant_destination.as_ref().map(|(p,_)| p),
        "width":processed.width,"height":processed.height,"profile":generated.profile,"model":generated.model,
        "reference_images_sent": !args.reference_image_paths.is_empty(),
        "note":format!("{note}. Decide whether to adopt this material or use it as reference; do not substitute it for an HTML UI mock.")});
    Ok(
        tools::ToolResult::success(result.to_string()).with_image(tools::ToolImage {
            media_type: "image/png".into(),
            data: processed.preview,
            path: Some(path),
        }),
    )
}

fn save(directory: &std::fs::File, path: &Path, bytes: &[u8]) -> Result<(), String> {
    crate::artifacts::source_io::store_image(
        directory,
        Path::new(path.file_name().ok_or("missing filename")?),
        bytes,
    )
}
