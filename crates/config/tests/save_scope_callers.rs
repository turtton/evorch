//! Production settings editors must declare their real layered save scope.

use std::path::{Path, PathBuf};

fn rust_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name == "tests"
            || name.ends_with("_tests.rs")
            || name.ends_with("_test.rs")
            || name.starts_with("test_")
        {
            continue;
        }
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn standalone_save_entry_points_are_confined_to_explicit_config_api_adapters() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    // Only the config API itself may implement the explicit standalone contract.
    // Runtime/GUI/evaluator code edits layered user settings, not isolated files.
    let adapters = [
        "config/src/lib.rs",
        "config/src/save_context.rs",
        "config/src/save.rs",
        "config/src/save_agents.rs",
        "config/src/save_codex.rs",
        "config/src/save_role_profiles.rs",
        "config/src/save_routing.rs",
        "config/src/save_sandbox.rs",
        "config/src/save_self_improvement.rs",
        "config/src/save_subscription.rs",
    ];
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    for path in files {
        let relative = path.strip_prefix(crates).unwrap().to_string_lossy();
        if adapters.contains(&relative.as_ref()) {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "StandaloneSaveContext",
            "SaveScope::Standalone",
            "config::save_",
            "config::save::save_",
            "config::delete_provider(",
            "config::delete_role_profile(",
            "config::preview_role_profiles_bindings(",
        ] {
            assert!(
                !production.contains(forbidden),
                "{relative} bypasses the layered save boundary via {forbidden}"
            );
        }
    }
}
