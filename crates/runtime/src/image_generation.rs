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

    fn variant_path(&self, output_path: &Path) -> Option<PathBuf> {
        let variant = self.variant.as_ref()?;
        let mut stem = output_path.file_stem()?.to_os_string();
        let extension = match variant.format {
            VariantFormat::Jpeg => "jpg",
            VariantFormat::Png => "png",
        };
        stem.push(format!(".{}.{extension}", variant.max_side));
        Some(output_path.with_file_name(stem))
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

pub(crate) fn guard_team_claims(state: &LoopState, input: &Value) -> Result<(), String> {
    let args: Args = crate::meta::parse(input.clone())?;
    args.validate()?;
    let workspace = state
        .shared
        .executor
        .default_cwd()
        .ok_or("this run has no workspace")?;
    let roots = crate::artifacts::SourceRoots::new(&workspace, state.scratch_dir())?;
    let (path, _) = roots.destination(&args.output_path)?;
    check_team_paths(
        state,
        &workspace,
        &path,
        args.variant_path(&path).as_deref(),
    )
}

fn check_team_paths(
    state: &LoopState,
    workspace: &Path,
    path: &Path,
    variant: Option<&Path>,
) -> Result<(), String> {
    let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
    for path in std::iter::once(path).chain(variant) {
        // Claims are workspace-relative, just like write/edit. Absolute output
        // paths cannot bypass the board and scratch has no claimable namespace.
        let relative = path.strip_prefix(&workspace).unwrap_or(path);
        state.guard_team_artifact("write", &json!({"path": relative}))?;
    }
    Ok(())
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
    let (path, directory) = roots.destination(&args.output_path)?;
    let variant_path = args.variant_path(&path);
    check_team_paths(state, &workspace, &path, variant_path.as_deref())?;
    if let Some(path) = &variant_path {
        crate::artifacts::source_io::validate_image_destination(&directory, filename(path)?)?;
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
        attempts: None,
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
    check_team_paths(state, &workspace, &path, variant_path.as_deref())?;
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
    let saved = save_generated(
        &state.task.publication,
        &directory,
        (&path, &original),
        variant_path.as_deref().zip(processed.variant.as_deref()),
        || {
            if state.interrupted().is_some() {
                return Err("image generation interrupted; no files saved".into());
            }
            check_team_paths(state, &workspace, &path, variant_path.as_deref())?;
            Ok(())
        },
    )?;
    Ok(generation_result(
        &args,
        &path,
        variant_path.as_deref(),
        &processed,
        &generated.profile,
        &generated.model,
        &saved,
    ))
}

fn generation_result(
    args: &Args,
    path: &Path,
    variant_path: Option<&Path>,
    processed: &processing::Outputs,
    profile: &str,
    model: &str,
    saved: &SaveOutcome,
) -> tools::ToolResult {
    let note = if args.reference_image_paths.is_empty() {
        "No reference images sent. Generation was sent to OpenAI."
    } else {
        "reference images were sent to OpenAI"
    };
    let result = json!({"path":path,"variant_path": if matches!(saved, SaveOutcome::Complete) { variant_path } else { None },
        "status": if matches!(saved, SaveOutcome::Complete) { "saved" } else { "partial_save" },
        "variant_failure": match &saved {
            SaveOutcome::Complete => None,
            SaveOutcome::VariantFailed(error) => Some(json!({"path":variant_path,"error":error})),
        },
        "width":processed.width,"height":processed.height,"profile":profile,"model":model,
        "reference_images_sent": !args.reference_image_paths.is_empty(),
        "note":format!("{note}. Decide whether to adopt this material or use it as reference; do not substitute it for an HTML UI mock.")});
    let result = if matches!(saved, SaveOutcome::Complete) {
        tools::ToolResult::success(result.to_string())
    } else {
        tools::ToolResult::error(result.to_string())
    };
    result.with_image(tools::ToolImage {
        media_type: "image/png".into(),
        data: processed.preview.clone(),
        path: Some(path.to_owned()),
    })
}

fn filename(path: &Path) -> Result<&Path, String> {
    Ok(Path::new(path.file_name().ok_or("missing filename")?))
}

#[derive(Debug)]
pub(crate) enum SaveOutcome {
    Complete,
    VariantFailed(String),
}

pub(crate) fn save_generated(
    publication: &crate::runtime::PublicationToken,
    directory: &std::fs::File,
    original: (&Path, &[u8]),
    variant: Option<(&Path, &[u8])>,
    before_commit: impl FnOnce() -> Result<(), String>,
) -> Result<SaveOutcome, String> {
    use crate::artifacts::source_io::{stage_image, validate_image_destination};
    let staged_variant = variant
        .map(|(_, bytes)| stage_image(directory, bytes))
        .transpose()?;
    let staged_original = stage_image(directory, original.1)?;
    // Deterministic seam after all writes/syncs, before the cancellation fence.
    before_commit()?;
    publication.commit(|| {
        validate_image_destination(directory, filename(original.0)?)?;
        if let Some((path, _)) = variant {
            validate_image_destination(directory, filename(path)?)?;
        }
        // Publish original first: an original failure cannot leave a variant.
        staged_original.publish(filename(original.0)?)?;
        if let Some(staged) = &staged_variant {
            let path = variant.expect("staged variant").0;
            if let Err(error) = staged.publish(filename(path)?) {
                return Ok(SaveOutcome::VariantFailed(error));
            }
        }
        Ok(SaveOutcome::Complete)
    })
}

#[cfg(test)]
fn save(directory: &std::fs::File, path: &Path, bytes: &[u8]) -> Result<(), String> {
    crate::artifacts::source_io::store_image(
        directory,
        Path::new(path.file_name().ok_or("missing filename")?),
        bytes,
    )
}
