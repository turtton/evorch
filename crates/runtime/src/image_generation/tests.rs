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
            a.variant_path().unwrap(),
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
