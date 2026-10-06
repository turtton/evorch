//! 保存前のファイル上書きとレイヤー優先順位の検証。

use std::collections::BTreeMap;

use config::types::agents::{explicit_refs, rename_logical_model_refs};
use config::{Config, LoadOptions};

fn options(root: &std::path::Path) -> LoadOptions {
    LoadOptions {
        project_dir: Some(root.join("project")),
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

#[test]
fn override_synthesizes_missing_main_without_writing() {
    // Given: the config directory and main file do not exist.
    let temp = tempfile::tempdir().expect("temp");
    let mut options = options(temp.path());
    let path = temp.path().join("user/config.toml");
    options.file_overrides.insert(
        path.clone(),
        toml::from_str("[agents.worker]\nlogical_model = 'virtual'\n").expect("candidate"),
    );
    // When: loading with an in-memory main file.
    let loaded = Config::load(&options).expect("load");
    // Then: the override is effective without any filesystem mutation.
    assert_eq!(
        loaded.agents.worker.logical_model.as_deref(),
        Some("virtual")
    );
    assert!(!path.parent().expect("user").exists());
}

#[test]
fn override_replaces_disk_content_instead_of_merging_it() {
    // Given: the disk has a different binding and a field omitted by the candidate.
    let temp = tempfile::tempdir().expect("temp");
    let mut options = options(temp.path());
    let path = temp.path().join("user/config.toml");
    std::fs::create_dir_all(path.parent().expect("user")).expect("directory");
    let original = "[agents.worker]\nlogical_model = 'disk'\npreset = 'disk-only'\n";
    std::fs::write(&path, original).expect("disk config");
    options.file_overrides.insert(
        path.clone(),
        toml::from_str("[agents.worker]\nlogical_model = 'candidate'\n").expect("candidate"),
    );
    // When: loading the candidate.
    let loaded = Config::load(&options).expect("load");
    // Then: no fields from the replaced file leak into the effective config.
    assert_eq!(
        loaded.agents.worker.logical_model.as_deref(),
        Some("candidate")
    );
    assert_eq!(loaded.agents.worker.preset, None);
    assert_eq!(std::fs::read_to_string(path).expect("disk"), original);
}

#[test]
fn non_overridden_files_still_load_and_dropins_can_be_overridden() {
    // Given: a main override, one replaced drop-in, and one ordinary drop-in.
    let temp = tempfile::tempdir().expect("temp");
    let mut options = options(temp.path());
    let user = temp.path().join("user");
    std::fs::create_dir_all(user.join("config.d")).expect("directory");
    std::fs::write(user.join("config.d/00-replaced.toml"), "[invalid TOML").expect("replaced file");
    std::fs::write(
        user.join("config.d/10-extra.toml"),
        "version = 1\n[agents.reviewer]\nlogical_model = 'review'\n",
    )
    .expect("ordinary drop-in");
    options.file_overrides = BTreeMap::from([
        (
            user.join("config.toml"),
            toml::from_str("[agents.worker]\nlogical_model = 'main'\n").expect("main"),
        ),
        (
            user.join("config.d/00-replaced.toml"),
            toml::from_str("[agents.worker]\nlogical_model = 'drop-in'\n").expect("drop-in"),
        ),
    ]);
    // When: loading in normal file order.
    let loaded = Config::load(&options).expect("load");
    // Then: replacements occupy the original layer positions; other files still migrate and load.
    assert_eq!(
        loaded.agents.worker.logical_model.as_deref(),
        Some("drop-in")
    );
    assert_eq!(
        loaded.agents.reviewer.logical_model.as_deref(),
        Some("review")
    );
    assert_eq!(loaded.version, config::CURRENT_VERSION);
}

#[test]
fn main_old_route_survives_but_does_not_shadow_dropin_renamed_binding() {
    // Given: a lower-layer route and explicit binding, plus an unrelated binding.
    let temp = tempfile::tempdir().expect("temp");
    let mut options = options(temp.path());
    let user = temp.path().join("user");
    std::fs::create_dir_all(&user).expect("user directory");
    std::fs::write(
        user.join("config.toml"),
        "[routing.routes]\nold = [{profile = 'local'}]\n\
         [agents.worker]\nlogical_model = 'old'\n\
         [agents.reviewer]\nlogical_model = 'review'\n",
    )
    .expect("user config");
    let before = Config::load(&options).expect("before");
    let mut renamed_agents = before.agents.clone();
    rename_logical_model_refs(&mut renamed_agents, &[("old".into(), "new".into())].into());
    let mut candidate: toml::Value =
        toml::from_str("[routing.routes]\nnew = [{profile = 'local'}]\n").expect("candidate");
    candidate.as_table_mut().expect("table").insert(
        "agents".into(),
        toml::Value::try_from(&renamed_agents).expect("agents"),
    );
    // Overrides replace drop-ins in place, so the candidate's file must already be listed.
    let dropin = user.join("config.d/50-candidate.toml");
    std::fs::create_dir_all(user.join("config.d")).expect("drop-in directory");
    std::fs::write(&dropin, "").expect("placeholder drop-in");
    options.file_overrides.insert(dropin, candidate);
    // When: simulating a rename in a later drop-in.
    let effective = Config::load(&options).expect("effective");
    let before_refs: BTreeMap<_, _> = explicit_refs(&before.agents).into_iter().collect();
    let effective_refs: BTreeMap<_, _> = explicit_refs(&effective.agents).into_iter().collect();
    // Then: the old route remains visible, but every rewritten binding passes the preflight check.
    assert!(effective.routing.routes.contains_key("old"));
    assert!(effective.routing.routes.contains_key("new"));
    assert_eq!(
        effective.agents.reviewer.logical_model.as_deref(),
        Some("review")
    );
    for (address, new_name) in explicit_refs(&renamed_agents) {
        if before_refs.get(&address) != Some(&new_name) {
            assert_eq!(effective_refs.get(&address), Some(&new_name));
        }
    }
    assert_eq!(
        effective.agents.worker.logical_model.as_deref(),
        Some("new")
    );
}

#[test]
fn env_and_cli_still_shadow_file_overrides() {
    // Given: a candidate binding rewritten to new, shadowed by either env or CLI.
    for cli in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let mut options = options(temp.path());
        options.file_overrides.insert(
            temp.path().join("user/config.toml"),
            toml::from_str("[agents.worker]\nlogical_model = 'new'\n").expect("candidate"),
        );
        if cli {
            options.cli_overrides =
                Some(toml::from_str("[agents.worker]\nlogical_model = 'old'\n").expect("CLI"));
        } else {
            options.read_env = true;
            options.env = Some(BTreeMap::from([(
                "EVORCH_AGENTS__WORKER__LOGICAL_MODEL".into(),
                "old".into(),
            )]));
        }
        // When: loading the simulated effective config.
        let effective = Config::load(&options).expect("effective");
        // Then: the higher layer is not bypassed by the file override.
        assert_eq!(
            effective.agents.worker.logical_model.as_deref(),
            Some("old")
        );
    }
}
