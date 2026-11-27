use super::*;

fn overlay(text: &str) -> Overlay {
    Overlay {
        text: text.into(),
        value: toml::from_str(text).unwrap(),
    }
}

fn user_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "version = 3\n").unwrap();
    dir
}

#[test]
fn the_candidate_side_is_the_common_overlay_with_the_candidate_over_it() {
    // Given: a common overlay and a candidate that overrides one of its keys and adds another.
    let common = overlay("[agents.worker.generation]\ntemperature = 0.1\ntop_p = 0.9\n");
    let candidate = overlay("[agents.worker.generation]\ntemperature = 0.5\n");

    // When: both sides are composed.
    let (baseline, composed) = compose(Some(&common), Some(&candidate));

    // Then: the baseline is the common overlay alone; the candidate wins where it speaks and
    // keeps the rest of the common overlay.
    assert_eq!(baseline, Some(common.value.clone()));
    let generation = &composed.unwrap()["agents"]["worker"]["generation"];
    assert_eq!(generation["temperature"].as_float(), Some(0.5));
    assert_eq!(generation["top_p"].as_float(), Some(0.9));
}

#[test]
fn there_is_no_candidate_side_without_a_candidate() {
    // Given: only a common overlay, then only a candidate.
    let common = overlay("[agents.worker.generation]\ntemperature = 0.1\n");
    let candidate = overlay("[agents.worker.generation]\ntemperature = 0.5\n");

    // When / Then: the baseline gets the common overlay only, the candidate gets itself.
    assert_eq!(
        compose(Some(&common), None),
        (Some(common.value.clone()), None)
    );
    assert_eq!(
        compose(None, Some(&candidate)),
        (None, Some(candidate.value.clone()))
    );
}

#[test]
fn the_digest_follows_the_sample_the_overlays_the_configuration_and_the_presets() {
    // Given: the inputs of one evaluation.
    let user = user_dir();
    let common = overlay("[agents.worker.generation]\ntemperature = 0.1\n");
    let candidate = overlay("[agents.worker.generation]\ntemperature = 0.5\n");
    let digest = |sample: &str, common: &Overlay, candidate: &Overlay| {
        collect(
            Some(user.path()),
            user.path(),
            Some(sample.into()),
            Some(common),
            Some(candidate),
        )
        .unwrap()
        .digest()
    };
    let approved = digest("sample-a", &common, &candidate);

    // When / Then: the same inputs hash the same, and a change to any one of them does not.
    assert_eq!(approved, digest("sample-a", &common, &candidate));
    assert_ne!(approved, digest("sample-b", &common, &candidate));
    assert_ne!(
        approved,
        digest(
            "sample-a",
            &overlay("[agents.worker.generation]\ntemperature = 0.2\n"),
            &candidate
        )
    );
    assert_ne!(
        approved,
        digest(
            "sample-a",
            &common,
            &overlay("[agents.worker.generation]\ntemperature = 0.6\n")
        )
    );
    std::fs::write(
        user.path().join("config.toml"),
        "version = 3\n[eval]\nrepeat_target = 7\n",
    )
    .unwrap();
    assert_ne!(approved, digest("sample-a", &common, &candidate));
    std::fs::write(user.path().join("config.toml"), "version = 3\n").unwrap();
    std::fs::create_dir(user.path().join("presets")).unwrap();
    std::fs::write(
        user.path().join("presets/role-worker.md"),
        "edited after approval",
    )
    .unwrap();
    assert_ne!(approved, digest("sample-a", &common, &candidate));
}

#[test]
fn the_guard_refuses_inputs_that_changed_after_they_were_pinned() {
    // Given: a guard pinned on the current inputs.
    let user = user_dir();
    let candidate = overlay("[agents.worker.generation]\ntemperature = 0.5\n");
    let pinned = collect(
        Some(user.path()),
        user.path(),
        Some("sample-a".into()),
        None,
        Some(&candidate),
    )
    .unwrap();
    let guard = Guard {
        user_config_dir: Some(user.path().into()),
        project_root: user.path().into(),
        common: None,
        candidate: Some(candidate),
        pinned,
    };
    guard.verify().unwrap();

    // When: a prompt preset is edited between two replays.
    std::fs::create_dir(user.path().join("presets")).unwrap();
    std::fs::write(user.path().join("presets/role-worker.md"), "edited mid-run").unwrap();

    // Then: the next replay is refused, naming what changed.
    let refused = guard.verify().unwrap_err();
    assert!(
        refused.contains("baseline prompt presets") && refused.contains("candidate prompt presets"),
        "{refused}"
    );
}
