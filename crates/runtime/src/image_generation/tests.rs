use super::*;
use crate::artifacts::SourceRoots;
use crate::{ExecutionPolicy, Role, RunConfig};
use base64::Engine;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(width, height)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
fn args() -> Args {
    serde_json::from_value(json!({"prompt":"material", "output_path":"material.png"})).unwrap()
}

#[test]
fn ac6_policy_matches_render_and_requires_nonempty_frozen_chain() {
    for role in [
        Role::Worker,
        Role::Orchestrator,
        Role::Reviewer,
        Role::Advisor,
    ] {
        for category in [None, Some("visual"), Some("conversation"), Some("general")] {
            for root in [true, false] {
                for conversation in [true, false] {
                    for enabled in [true, false] {
                        let config = RunConfig {
                            category: category.map(Into::into),
                            conversation,
                            ..Default::default()
                        };
                        let expected = crate::artifacts::may_render(role, &config, root) && enabled;
                        let policy = ExecutionPolicy::for_role(role)
                            .for_run_config(&config, root)
                            .with_image_generation(enabled);
                        assert_eq!(policy.authorize("generate_image").is_ok(), expected);
                    }
                }
            }
        }
    }
}

#[test]
fn ac8_ac11_argument_boundaries_and_variant_names() {
    let mut a = args();
    assert!(a.validate().is_ok());
    for side in [0, 63, 2049, u32::MAX] {
        a.variant = Some(Variant {
            max_side: side,
            format: VariantFormat::Png,
        });
        assert!(a.validate().is_err());
    }
    for side in [64, 2048] {
        a.variant = Some(Variant {
            max_side: side,
            format: VariantFormat::Jpeg,
        });
        assert!(a.validate().is_ok());
        assert_eq!(
            a.variant_path(&a.output_path).unwrap(),
            PathBuf::from(format!("material.{side}.jpg"))
        );
    }
    a.transparent_background = true;
    assert!(a.validate().is_err());
    a.variant.as_mut().unwrap().format = VariantFormat::Png;
    assert!(a.validate().is_ok());
    a.reference_image_paths = vec!["a.png".into(); 6];
    assert!(a.validate().is_err());
    a.reference_image_paths.truncate(5);
    assert!(a.validate().is_ok());
    a.output_path = "bad.jpg".into();
    assert!(a.validate().is_err());
    a.output_path = "material.png".into();
    a.prompt = " ".into();
    assert!(a.validate().is_err());
    assert!(
        serde_json::from_value::<Args>(
            json!({"prompt":"p","output_path":"a.png","category":"visual"})
        )
        .is_err()
    );
}

#[test]
fn ac9_output_and_reference_boundary_symlink_other_thread_and_atomic_overwrite() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let roots = SourceRoots::new(workspace.path(), Some(scratch.path())).unwrap();
    let bytes = png(32, 16);
    for root in [workspace.path(), scratch.path()] {
        let p = root.join("material.png");
        let (path, dir) = roots.destination(&p).unwrap();
        save(&dir, &path, &bytes).unwrap();
        save(&dir, &path, &bytes).unwrap();
        assert_eq!(roots.read_image(&p).unwrap().2, bytes);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
    }
    assert!(roots.destination(&other.path().join("out.png")).is_err());
    std::fs::write(other.path().join("secret.png"), &bytes).unwrap();
    assert!(roots.read_image(&other.path().join("secret.png")).is_err());
    std::os::unix::fs::symlink(other.path(), workspace.path().join("escape")).unwrap();
    assert!(roots.destination(Path::new("escape/out.png")).is_err());
    assert!(roots.read_image(Path::new("escape/secret.png")).is_err());
    std::os::unix::fs::symlink(
        other.path().join("secret.png"),
        workspace.path().join("link.png"),
    )
    .unwrap();
    assert!(roots.destination(Path::new("link.png")).is_err());
    assert!(roots.destination(Path::new("missing/out.png")).is_err());
    // A swapped final symlink is atomically replaced, not followed.
    let (path, dir) = roots.destination(Path::new("race.png")).unwrap();
    std::os::unix::fs::symlink(other.path().join("secret.png"), &path).unwrap();
    save(&dir, &path, b"new").unwrap();
    assert_eq!(
        std::fs::read(other.path().join("secret.png")).unwrap(),
        bytes
    );
    assert_eq!(std::fs::read(path).unwrap(), b"new");
}

#[test]
fn ac10_reference_magic_size_and_resize_to_2048() {
    let workspace = tempfile::tempdir().unwrap();
    let roots = SourceRoots::new(workspace.path(), None).unwrap();
    let path = workspace.path().join("image.png");
    std::fs::write(&path, png(2050, 10)).unwrap();
    let references = processing::references(&roots, std::slice::from_ref(&path)).unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&references[0].data)
        .unwrap();
    let image = image::load_from_memory(&bytes).unwrap();
    assert_eq!(image.width(), 2048);
    assert_eq!(references[0].media_type, "image/png");
    std::fs::write(&path, b"GIF89a mislabeled").unwrap();
    assert!(processing::references(&roots, std::slice::from_ref(&path)).is_err());
    let mut bytes = png(1, 1);
    bytes.resize(8 * 1024 * 1024, 0);
    std::fs::write(&path, &bytes).unwrap();
    assert!(roots.read_image(&path).is_ok());
    bytes.push(0);
    std::fs::write(&path, &bytes).unwrap();
    assert!(roots.read_image(&path).is_err());
}

#[test]
fn ac11_ac12_variant_and_preview_dimensions_and_formats() {
    for format in [VariantFormat::Jpeg, VariantFormat::Png] {
        let output = processing::outputs(
            &png(1100, 550),
            Some(&Variant {
                max_side: 64,
                format,
            }),
        )
        .unwrap();
        let variant = image::load_from_memory(output.variant.as_ref().unwrap()).unwrap();
        assert_eq!((variant.width(), variant.height()), (64, 32));
        let preview = base64::engine::general_purpose::STANDARD
            .decode(&output.preview)
            .unwrap();
        let preview = image::load_from_memory(&preview).unwrap();
        assert_eq!((preview.width(), preview.height()), (1024, 512));
        assert_eq!((output.width, output.height), (1100, 550));
    }
}

#[test]
fn ac9_descriptor_relative_output_refuses_ancestor_swap() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let roots = SourceRoots::new(workspace.path(), None).unwrap();
    std::fs::create_dir(workspace.path().join("images")).unwrap();
    let (path, directory) = roots.destination(Path::new("images/output.png")).unwrap();
    std::fs::rename(
        workspace.path().join("images"),
        workspace.path().join("pinned"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), workspace.path().join("images")).unwrap();
    assert!(roots.destination(Path::new("images/other.png")).is_err());
    save(&directory, &path, b"png").unwrap();
    assert!(!outside.path().join("output.png").exists());
    assert_eq!(
        std::fs::read(workspace.path().join("pinned/output.png")).unwrap(),
        b"png"
    );
}

#[test]
fn ac11_ac12_small_images_are_not_upscaled() {
    let output = processing::outputs(
        &png(10, 5),
        Some(&Variant {
            max_side: 64,
            format: VariantFormat::Png,
        }),
    )
    .unwrap();
    let variant = image::load_from_memory(output.variant.as_ref().unwrap()).unwrap();
    let preview = image::load_from_memory(
        &base64::engine::general_purpose::STANDARD
            .decode(output.preview)
            .unwrap(),
    )
    .unwrap();
    assert_eq!((variant.width(), variant.height()), (10, 5));
    assert_eq!((preview.width(), preview.height()), (10, 5));
}

#[test]
fn ac13_tool_description_discloses_destination_quota_and_agent_discretion() {
    let spec = spec("generate_image");
    for text in [
        "OpenAI",
        "Reference images",
        "quota",
        "ignored by Codex",
        "adopt",
        "UI mocks",
        "HTML",
    ] {
        assert!(spec.description.contains(text));
    }
    jsonschema::validator_for(&spec.input_schema).unwrap();
}

struct IdleModel;
#[async_trait::async_trait]
impl crate::AgentModel for IdleModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "idle".into()
    }
}
fn saving_runtime() -> (AgentRuntime, crate::RunId) {
    let bus = std::sync::Arc::new(event_bus::EventBus::new(16));
    let runtime = AgentRuntime::new(
        bus.clone(),
        std::sync::Arc::new(tools::ToolExecutor::new(bus)),
        std::sync::Arc::new(IdleModel),
    );
    let run = runtime.reserve_run_id();
    runtime.spawn_reserved(run, None, Role::Worker, "idle", RunConfig::default());
    (runtime, run)
}

#[tokio::test]
async fn ac15_cancel_after_sync_prevents_publish_overwrite_and_removes_staging() {
    for subtree in [false, true] {
        for overwrite in [false, true] {
            let workspace = tempfile::tempdir().unwrap();
            let roots = SourceRoots::new(workspace.path(), None).unwrap();
            let (path, dir) = roots.destination(Path::new("hero.png")).unwrap();
            let variant = path.with_file_name("hero.64.png");
            if overwrite {
                std::fs::write(&path, b"old original").unwrap();
                std::fs::write(&variant, b"old variant").unwrap();
            }
            let (runtime, run) = saving_runtime();
            let result = save_generated(
                &runtime,
                run,
                &dir,
                (&path, b"new original"),
                Some((&variant, b"new variant")),
                || {
                    assert_eq!(
                        std::fs::read_dir(workspace.path()).unwrap().count(),
                        if overwrite { 4 } else { 2 }
                    );
                    std::thread::scope(|scope| {
                        scope
                            .spawn(|| {
                                if subtree {
                                    runtime.cancel_subtree(run).unwrap();
                                } else {
                                    runtime.cancel(run).unwrap();
                                }
                            })
                            .join()
                            .unwrap();
                    });
                    Ok(())
                },
            );
            assert!(result.unwrap_err().contains("interrupted"));
            if overwrite {
                assert_eq!(std::fs::read(&path).unwrap(), b"old original");
                assert_eq!(std::fs::read(&variant).unwrap(), b"old variant");
            } else {
                assert!(!path.exists());
                assert!(!variant.exists());
            }
            assert_eq!(
                std::fs::read_dir(workspace.path()).unwrap().count(),
                if overwrite { 2 } else { 0 }
            );
        }
    }
}

#[tokio::test]
async fn ac9_original_staging_failure_preserves_existing_pair_and_cleans_temps() {
    let workspace = tempfile::tempdir().unwrap();
    let roots = SourceRoots::new(workspace.path(), None).unwrap();
    let (path, dir) = roots.destination(Path::new("hero.png")).unwrap();
    let variant = path.with_file_name("hero.64.png");
    std::fs::write(&path, b"old original").unwrap();
    std::fs::write(&variant, b"old variant").unwrap();
    let (runtime, run) = saving_runtime();
    let result = crate::artifacts::source_io::with_stage_failure(2, || {
        save_generated(
            &runtime,
            run,
            &dir,
            (&path, b"new original"),
            Some((&variant, b"new variant")),
            || panic!("must not reach publication"),
        )
    });
    assert!(result.unwrap_err().contains("injected staging failure"));
    assert_eq!(std::fs::read(&path).unwrap(), b"old original");
    assert_eq!(std::fs::read(&variant).unwrap(), b"old variant");
    assert_eq!(std::fs::read_dir(workspace.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn ac9_original_publish_failure_leaves_no_variant_or_staging() {
    for overwrite in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let roots = SourceRoots::new(workspace.path(), None).unwrap();
        let (path, dir) = roots.destination(Path::new("hero.png")).unwrap();
        let variant = path.with_file_name("hero.64.png");
        if overwrite {
            std::fs::write(&path, b"old original").unwrap();
            std::fs::write(&variant, b"old variant").unwrap();
        }
        let (runtime, run) = saving_runtime();
        let result = crate::artifacts::source_io::with_publish_failure(1, || {
            save_generated(
                &runtime,
                run,
                &dir,
                (&path, b"original"),
                Some((&variant, b"variant")),
                || Ok(()),
            )
        });
        let error = result.unwrap_err();
        assert!(error.contains("injected renameat failure"));
        assert!(error.contains("hero.png"));
        if overwrite {
            assert_eq!(std::fs::read(&path).unwrap(), b"old original");
            assert_eq!(std::fs::read(&variant).unwrap(), b"old variant");
        } else {
            assert!(!path.exists());
            assert!(!variant.exists());
        }
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            if overwrite { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn ac9_variant_rename_failure_reports_partial_save_and_preserves_old_variant() {
    for overwrite in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let roots = SourceRoots::new(workspace.path(), None).unwrap();
        let (path, dir) = roots.destination(Path::new("hero.png")).unwrap();
        let variant = path.with_file_name("hero.64.png");
        if overwrite {
            std::fs::write(&path, b"old original").unwrap();
            std::fs::write(&variant, b"old variant").unwrap();
        }
        let (runtime, run) = saving_runtime();
        let original = png(128, 64);
        let mut args = args();
        args.variant = Some(Variant {
            max_side: 64,
            format: VariantFormat::Png,
        });
        args.reference_image_paths.push("reference.png".into());
        let processed = processing::outputs(&original, args.variant.as_ref()).unwrap();
        let saved = crate::artifacts::source_io::with_publish_failure(2, || {
            save_generated(
                &runtime,
                run,
                &dir,
                (&path, &original),
                Some((&variant, processed.variant.as_deref().unwrap())),
                || Ok(()),
            )
        })
        .unwrap();
        assert!(matches!(saved, SaveOutcome::VariantFailed(_)));
        let result = generation_result(
            &args,
            &path,
            Some(&variant),
            &processed,
            "images",
            "gpt-image-2",
            &saved,
        );
        assert!(result.is_error);
        let text: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(text["status"], "partial_save");
        assert_eq!(text["path"], json!(path));
        assert!(text["variant_path"].is_null());
        assert_eq!(text["variant_failure"]["path"], json!(variant));
        assert!(
            text["variant_failure"]["error"]
                .as_str()
                .unwrap()
                .contains("injected renameat failure")
        );
        assert_eq!(text["profile"], "images");
        assert_eq!(text["model"], "gpt-image-2");
        assert_eq!(text["width"], 128);
        assert_eq!(text["height"], 64);
        assert_eq!(text["reference_images_sent"], true);
        assert!(
            text["note"]
                .as_str()
                .unwrap()
                .contains("reference images were sent to OpenAI")
        );
        assert_eq!(result.images[0].data, processed.preview);
        assert_eq!(result.images[0].path.as_ref(), Some(&path));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        if overwrite {
            assert_eq!(std::fs::read(&variant).unwrap(), b"old variant");
        } else {
            assert!(!variant.exists());
        }
        assert_eq!(
            std::fs::read_dir(workspace.path()).unwrap().count(),
            if overwrite { 2 } else { 1 }
        );
    }
}

#[tokio::test]
async fn ac15_publication_gate_does_not_hold_global_spawn_or_cancel_locks() {
    let (runtime, run) = saving_runtime();
    runtime
        .commit_generated_images(run, || {
            // A different run can spawn and cancel while this run publishes.
            let other = runtime.reserve_run_id();
            runtime.spawn_reserved(other, None, Role::Worker, "idle", RunConfig::default());
            runtime.cancel(other).unwrap();
            runtime.cancel_subtree(other).unwrap();
            Ok(())
        })
        .unwrap();
    runtime.cancel(run).unwrap();
    assert!(runtime.commit_generated_images(run, || Ok(())).is_err());
}

#[tokio::test]
async fn ac15_cancel_waits_for_publication_that_already_won_the_gate() {
    for subtree in [false, true] {
        let (runtime, run) = saving_runtime();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            runtime
                .commit_generated_images(run, || {
                    let runtime = &runtime;
                    scope.spawn(move || {
                        started_tx.send(()).unwrap();
                        if subtree {
                            runtime.cancel_subtree(run).unwrap();
                        } else {
                            runtime.cancel(run).unwrap();
                        }
                        finished_tx.send(()).unwrap();
                    });
                    started_rx.recv().unwrap();
                    assert!(matches!(
                        finished_rx.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    ));
                    Ok(())
                })
                .unwrap();
            finished_rx.recv().unwrap();
        });
        assert!(runtime.commit_generated_images(run, || Ok(())).is_err());
    }
}
