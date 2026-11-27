//! The parts of the new GUI's screen-diff evidence that need no graphics adapter: the
//! scenarios build and show what they are about, the comparison tells the cases apart, and
//! the capture process's environment and font are checked. Drawing itself is the capture
//! job's work (`scripts/core-gui-visual/capture.sh`).

#[allow(dead_code)]
#[path = "../visual-support/mod.rs"]
mod visual;

use std::path::Path;

use image::{Rgba, RgbaImage};
use visual::{
    compare::{self, Status},
    environment, font,
    manifest::{self, Capture, EnvironmentRecord, FontRecord, Manifest, ScenarioRecord},
    scenarios,
};

fn solid(width: u32, height: u32, colour: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba(colour))
}

fn manifest_for(ids: &[(&str, &RgbaImage)], fixture: &str) -> Manifest {
    Manifest {
        schema: manifest::SCHEMA,
        fixture_sha256: fixture.into(),
        theme: "graphite".into(),
        dpi: 1.0,
        font: FontRecord {
            path: "/fonts/a.ttf".into(),
            sha256: "aa".into(),
        },
        environment: EnvironmentRecord {
            os: "linux".into(),
            arch: "x86_64".into(),
            wgpu_backend: Some("vulkan".into()),
        },
        scenarios: ids
            .iter()
            .map(|(id, image)| ScenarioRecord {
                id: (*id).into(),
                file: format!("{id}.png"),
                width: image.width(),
                height: image.height(),
                rgba_sha256: manifest::rgba_sha256(image),
            })
            .collect(),
    }
}

/// Writes a capture directory the way the tool does.
fn write_capture(dir: &Path, ids: &[(&str, &RgbaImage)], fixture: &str) -> Capture {
    std::fs::create_dir_all(dir).unwrap();
    for (id, image) in ids {
        manifest::write_png(&dir.join(format!("{id}.png")), image).unwrap();
    }
    let manifest = manifest_for(ids, fixture);
    std::fs::write(
        dir.join(manifest::MANIFEST_FILE),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
    manifest::load(dir).unwrap()
}

// Given: two captures of the same pixels / When: compared
// Then: every screen is unchanged and nothing is published
#[test]
fn identical_captures_report_no_difference() {
    let root = tempfile::tempdir().unwrap();
    let image = solid(40, 30, [10, 20, 30, 255]);
    let before = write_capture(
        &root.path().join("b"),
        &[("a", &image), ("b", &image)],
        "f1",
    );
    let after = write_capture(
        &root.path().join("a"),
        &[("a", &image), ("b", &image)],
        "f1",
    );
    let report = compare::compare(Some(&before), &after, &root.path().join("out")).unwrap();
    assert!(report.baseline && !report.has_difference() && !report.fixture_changed);
    assert!(
        report
            .entries
            .iter()
            .all(|e| e.status == Status::Unchanged && e.diff.is_none())
    );
    compare::require_identical(&before, &after).unwrap();
}

// Given: a screen that differs by one pixel / When: compared
// Then: it is changed with exactly that pixel, boxed and highlighted in the diff image,
//       and the unchanged screen is not shown
#[test]
fn one_changed_pixel_is_found_and_highlighted() {
    let root = tempfile::tempdir().unwrap();
    let same = solid(64, 48, [0, 0, 0, 255]);
    let old = solid(64, 48, [255, 255, 255, 255]);
    let mut new = old.clone();
    new.put_pixel(20, 10, Rgba([254, 255, 255, 255]));
    let before = write_capture(
        &root.path().join("b"),
        &[("calm", &same), ("busy", &old)],
        "f1",
    );
    let after = write_capture(
        &root.path().join("a"),
        &[("calm", &same), ("busy", &new)],
        "f1",
    );
    let out = root.path().join("out");
    let report = compare::compare(Some(&before), &after, &out).unwrap();
    let shown: Vec<_> = report.shown().collect();
    assert_eq!(shown.len(), 1);
    let entry = shown[0];
    assert_eq!(
        (entry.id.as_str(), entry.status, entry.changed_pixels),
        ("busy", Status::Changed, 1)
    );
    assert_eq!(entry.bounds, Some([20, 10, 21, 11]));
    let diff = image::open(out.join(entry.diff.as_ref().unwrap()))
        .unwrap()
        .to_rgba8();
    assert_eq!(
        diff.get_pixel(20, 10),
        &Rgba([255, 0, 90, 255]),
        "the pixel itself is highlighted"
    );
    assert_ne!(
        diff.get_pixel(40, 40),
        &Rgba([255, 0, 90, 255]),
        "far from the change the image is only faded"
    );
    assert!(
        compare::require_identical(&before, &after).is_err(),
        "a repeat that differs is not deterministic"
    );
    assert!(
        out.join(entry.before.as_ref().unwrap()).exists()
            && out.join(entry.after.as_ref().unwrap()).exists()
    );
}

// Given: a fully different screen / Then: every pixel counts
#[test]
fn a_fully_different_screen_counts_every_pixel() {
    let root = tempfile::tempdir().unwrap();
    let before = write_capture(
        &root.path().join("b"),
        &[("a", &solid(16, 8, [0, 0, 0, 255]))],
        "f1",
    );
    let after = write_capture(
        &root.path().join("a"),
        &[("a", &solid(16, 8, [255, 255, 255, 255]))],
        "f1",
    );
    let report = compare::compare(Some(&before), &after, &root.path().join("out")).unwrap();
    assert_eq!(report.entries[0].changed_pixels, 16 * 8);
}

// Given: a screen only one side has, and the fixture changed
// Then: added and removed are told apart from changes, and the fixture change is reported
#[test]
fn added_and_removed_screens_are_reported_not_hidden() {
    let root = tempfile::tempdir().unwrap();
    let image = solid(8, 8, [1, 2, 3, 255]);
    let before = write_capture(
        &root.path().join("b"),
        &[("kept", &image), ("old", &image)],
        "f1",
    );
    let after = write_capture(
        &root.path().join("a"),
        &[("kept", &image), ("new", &image)],
        "f2",
    );
    let report = compare::compare(Some(&before), &after, &root.path().join("out")).unwrap();
    let status = |id: &str| report.entries.iter().find(|e| e.id == id).unwrap().status;
    assert_eq!(
        (status("kept"), status("old"), status("new")),
        (Status::Unchanged, Status::Removed, Status::Added)
    );
    assert!(report.fixture_changed);
    let new = report.entries.iter().find(|e| e.id == "new").unwrap();
    assert!(
        new.before.is_none() && new.after.is_some() && new.diff.is_none(),
        "no invented before image"
    );
}

// Given: no baseline / Then: every screen is new, which is not "no difference"
#[test]
fn without_a_baseline_every_screen_is_new() {
    let root = tempfile::tempdir().unwrap();
    let image = solid(8, 8, [1, 2, 3, 255]);
    let after = write_capture(
        &root.path().join("a"),
        &[("a", &image), ("b", &image)],
        "f1",
    );
    let report = compare::compare(None, &after, &root.path().join("out")).unwrap();
    assert!(!report.baseline && report.has_difference());
    assert!(report.entries.iter().all(|e| e.status == Status::Added));
    assert_eq!(
        compare::read_report(&root.path().join("out")).unwrap(),
        report
    );
}

// Given: captures that cannot be compared / Then: an error, never a quiet "no difference"
#[test]
fn incomparable_captures_are_errors() {
    let root = tempfile::tempdir().unwrap();
    let small = solid(8, 8, [1, 2, 3, 255]);
    let big = solid(9, 8, [1, 2, 3, 255]);
    let a = write_capture(&root.path().join("a"), &[("x", &small)], "f1");
    let b = write_capture(&root.path().join("b"), &[("x", &big)], "f1");
    let out = root.path().join("out");
    assert!(
        compare::compare(Some(&a), &b, &out)
            .unwrap_err()
            .contains("not comparable")
    );

    let mut other_font = write_capture(&root.path().join("c"), &[("x", &small)], "f1");
    other_font.manifest.font.sha256 = "bb".into();
    assert!(
        compare::compare(Some(&a), &other_font, &out)
            .unwrap_err()
            .contains("font")
    );

    let mut other_dpi = write_capture(&root.path().join("d"), &[("x", &small)], "f1");
    other_dpi.manifest.dpi = 2.0;
    assert!(
        compare::compare(Some(&a), &other_dpi, &out)
            .unwrap_err()
            .contains("dpi")
    );

    let mut other_env = write_capture(&root.path().join("e"), &[("x", &small)], "f1");
    other_env.manifest.environment.arch = "aarch64".into();
    assert!(
        compare::compare(Some(&a), &other_env, &out)
            .unwrap_err()
            .contains("environments")
    );
}

// Given: broken capture directories / Then: loading names the problem
#[test]
fn broken_captures_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let image = solid(8, 8, [1, 2, 3, 255]);
    let dir = root.path().join("a");
    write_capture(&dir, &[("x", &image)], "f1");

    // a PNG that is not a PNG
    std::fs::write(dir.join("x.png"), b"not a png").unwrap();
    assert!(
        manifest::load(&dir)
            .unwrap_err()
            .contains("not a readable PNG")
    );

    // a valid PNG with other pixels than the manifest recorded
    manifest::write_png(&dir.join("x.png"), &solid(8, 8, [9, 9, 9, 255])).unwrap();
    assert!(manifest::load(&dir).unwrap_err().contains("pixel hash"));

    // a PNG of another size than the manifest states
    manifest::write_png(&dir.join("x.png"), &solid(9, 8, [1, 2, 3, 255])).unwrap();
    assert!(
        manifest::load(&dir)
            .unwrap_err()
            .contains("the manifest says")
    );

    // a missing file, a missing manifest, an unknown schema, an unsafe name
    std::fs::remove_file(dir.join("x.png")).unwrap();
    assert!(manifest::load(&dir).is_err());
    assert!(manifest::load(&root.path().join("nothing")).is_err());
    let mut unsafe_manifest = manifest_for(&[("x", &image)], "f1");
    unsafe_manifest.scenarios[0].file = "../x.png".into();
    std::fs::write(
        dir.join(manifest::MANIFEST_FILE),
        serde_json::to_string(&unsafe_manifest).unwrap(),
    )
    .unwrap();
    assert!(manifest::load(&dir).unwrap_err().contains("unsafe"));
    let mut future = manifest_for(&[("x", &image)], "f1");
    future.schema = manifest::SCHEMA + 1;
    std::fs::write(
        dir.join(manifest::MANIFEST_FILE),
        serde_json::to_string(&future).unwrap(),
    )
    .unwrap();
    assert!(manifest::load(&dir).unwrap_err().contains("schema"));
}

// Given: a PNG larger than the limit / Then: it is refused before it is decoded
#[test]
fn oversized_input_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let image = solid(8, 8, [1, 2, 3, 255]);
    let dir = root.path().join("a");
    write_capture(&dir, &[("x", &image)], "f1");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("x.png"))
        .unwrap();
    file.set_len(manifest::MAX_FILE_BYTES + 1).unwrap();
    assert!(manifest::load(&dir).unwrap_err().contains("larger than"));

    let wide = solid(manifest::MAX_DIMENSION + 1, 1, [0, 0, 0, 255]);
    let dir = root.path().join("wide");
    std::fs::create_dir_all(&dir).unwrap();
    manifest::write_png(&dir.join("x.png"), &wide).unwrap();
    std::fs::write(
        dir.join(manifest::MANIFEST_FILE),
        serde_json::to_string(&manifest_for(&[("x", &wide)], "f1")).unwrap(),
    )
    .unwrap();
    assert!(manifest::load(&dir).unwrap_err().contains("px"));
}

// Given: file names that are published / Then: the content is in the name, so a changed
//       image is a new URL and the same image keeps its URL
#[test]
fn published_names_carry_the_content_hash() {
    let root = tempfile::tempdir().unwrap();
    let a = write_capture(
        &root.path().join("a"),
        &[("x", &solid(8, 8, [0, 0, 0, 255]))],
        "f1",
    );
    let b = write_capture(
        &root.path().join("b"),
        &[("x", &solid(8, 8, [9, 9, 9, 255]))],
        "f1",
    );
    let first = compare::compare(Some(&a), &b, &root.path().join("o1")).unwrap();
    let again = compare::compare(Some(&a), &b, &root.path().join("o2")).unwrap();
    assert_eq!(first.entries, again.entries);
    let name = first.entries[0].after.as_ref().unwrap();
    assert!(
        name.strip_prefix("x-after-")
            .and_then(|s| s.strip_suffix(".png"))
            .is_some_and(|hash| hash.len() == 12 && hash.bytes().all(|c| c.is_ascii_hexdigit())),
        "{name}"
    );
}

// Given: variables of a runner / Then: only the allowlist passes, and a value is never echoed
#[test]
fn the_environment_allowlist_refuses_tokens_and_settings() {
    let bad = environment::violations([
        "HOME",
        "PATH",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "ACTIONS_RUNTIME_TOKEN",
        "EVORCH_VISUAL_SENTINEL",
        "FONTCONFIG_FILE",
        "AWS_SECRET_ACCESS_KEY",
    ]);
    assert_eq!(
        bad,
        [
            "ACTIONS_RUNTIME_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "GH_TOKEN",
            "GITHUB_TOKEN"
        ]
    );
    assert!(
        environment::violations([
            "HOME",
            "TMPDIR",
            "LD_LIBRARY_PATH",
            "WGPU_BACKEND",
            "EVORCH_VISUAL_X"
        ])
        .is_empty()
    );
}

// Given: a screen that draws a path of the process / Then: the leak is found
#[test]
fn a_drawn_path_of_the_process_is_a_leak() {
    let drawn = vec![
        "Add project".to_owned(),
        "opened /var/tmp/secret-home/.config".to_owned(),
    ];
    let found = environment::find_leak(&drawn, &["/var/tmp/secret-home".to_owned()]);
    assert_eq!(
        found.map(|(text, _)| text),
        Some("opened /var/tmp/secret-home/.config")
    );
    assert!(environment::find_leak(&drawn, &["/other".to_owned()]).is_none());
    assert!(
        environment::forbidden_texts(&[Path::new("/x")])
            .iter()
            .all(|t| t.len() >= 4),
        "too short a value would match everything"
    );
}

// Given: fc-match answers / Then: no answer or a failure is an error and a path is its output
#[test]
fn the_font_query_needs_an_answer() {
    use std::os::unix::process::ExitStatusExt;
    let output = |code: i32, stdout: &str| std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    };
    assert_eq!(
        font::resolve_path(&output(0, "/fonts/a.ttf\n")).unwrap(),
        "/fonts/a.ttf"
    );
    assert!(font::resolve_path(&output(0, "")).is_err());
    assert!(font::resolve_path(&output(1, "/fonts/a.ttf")).is_err());
}

// Given: the scenarios / Then: ids name files safely, are unique, and have the stated size
#[test]
fn scenarios_are_well_formed() {
    let ids: Vec<_> = scenarios::SCENARIOS.iter().map(|s| s.id).collect();
    assert!(ids.iter().all(|id| manifest::valid_id(id)));
    assert_eq!(
        ids.len(),
        ids.iter().collect::<std::collections::BTreeSet<_>>().len()
    );
    assert_eq!(scenarios::size(), (1440, 900));
    assert!(!manifest::valid_id("../x") && !manifest::valid_id("A") && !manifest::valid_id(""));
}

// Given: each scenario's fixture / When: the real screen is built
// Then: it asks only what the fixture answers, shows what the scenario is about, stops
//       asking to be redrawn, and draws nothing of this process's paths
#[test]
fn every_scenario_builds_and_shows_its_content() {
    let forbidden = environment::forbidden_texts(&[]);
    for spec in scenarios::SCENARIOS {
        let built = scenarios::build(spec.id, &forbidden)
            .unwrap_or_else(|error| panic!("{}: {error}", spec.id));
        let texts = built.texts();
        for label in spec.required {
            assert!(
                texts.iter().any(|t| t == label),
                "{} lacks {label:?}",
                spec.id
            );
        }
    }
}

// Given: a fixture whose screen would draw a forbidden text / Then: the build fails
#[test]
fn a_forbidden_text_on_screen_fails_the_build() {
    let error = scenarios::build("workspace-conversation", &["Add project".to_owned()])
        .err()
        .expect("a leak fails");
    assert!(error.contains("Add project"), "{error}");
}

// Given: an unknown scenario / Then: an error
#[test]
fn an_unknown_scenario_is_an_error() {
    assert!(scenarios::build("nope", &[]).is_err());
}
