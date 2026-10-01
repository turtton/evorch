//! 新プロジェクトレイアウトの読み込みと公開保存先ヘルパーの検証。

use std::path::Path;

use config::{Config, ConfigError, LoadOptions, PROJECT_CONFIG_DIR, project_main_config_path};

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    std::fs::write(path, content).expect("config file");
}

fn options(root: &Path) -> LoadOptions {
    LoadOptions {
        project_dir: Some(root.join("project")),
        user_config_dir: Some(root.join("user")),
        read_env: false,
        ..Default::default()
    }
}

fn load_both(options: &LoadOptions) -> [Config; 2] {
    [
        Config::load(options).expect("tolerant load"),
        Config::load_strict(options).expect("strict load"),
    ]
}

#[test]
fn project_main_path_uses_new_layout_without_creating_it() {
    let temp = tempfile::tempdir().expect("temp");
    let project = temp.path().join("missing-project");

    assert_eq!(PROJECT_CONFIG_DIR, ".evorch");
    assert_eq!(
        project_main_config_path(&project),
        project.join(".evorch/config.toml")
    );
    assert_eq!(
        project_main_config_path(Path::new("relative")),
        Path::new("relative/.evorch/config.toml")
    );
    assert!(!project.exists());

    write_file(&project.join("evorch.toml"), "");
    assert_eq!(
        project_main_config_path(&project),
        project.join(".evorch/config.toml"),
        "the default save path is independent of legacy files"
    );
    assert!(!project.join(".evorch").exists());
}

#[test]
fn new_main_and_dropins_load_in_lexicographic_order() {
    let temp = tempfile::tempdir().expect("temp");
    let options = options(temp.path());
    let dir = temp.path().join("project/.evorch");
    write_file(
        &dir.join("config.toml"),
        "[metrics]\nenabled = false\nretention_days = 7\n",
    );
    // Deliberately create the later-sorting drop-in first.
    write_file(
        &dir.join("config.d/90-last.toml"),
        "[metrics]\nretention_days = 19\n",
    );
    write_file(
        &dir.join("config.d/10-first.toml"),
        "[metrics]\nretention_days = 11\n",
    );
    write_file(&dir.join("config.d/99-ignored.txt"), "[invalid TOML");
    std::fs::create_dir_all(dir.join("config.d/99-directory.toml")).expect("directory");

    for loaded in load_both(&options) {
        assert!(!loaded.metrics.enabled, "main-only values survive");
        assert_eq!(loaded.metrics.retention_days, 19);
    }
}

#[test]
fn legacy_files_are_ignored_without_new_main() {
    for user_layer in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let options = options(temp.path());
        let project = temp.path().join("project");
        let mut expected = Config::default();
        if user_layer {
            write_file(
                &temp.path().join("user/config.toml"),
                "[metrics]\nretention_days = 7\n",
            );
            write_file(
                &temp.path().join("user/config.d/10-user.toml"),
                "[metrics]\nretention_days = 11\n",
            );
            expected.metrics.retention_days = 11;
        }
        write_file(
            &project.join("evorch.toml"),
            "[metrics]\nenabled = false\nretention_days = 99\n",
        );
        write_file(
            &project.join("config.d/90-legacy.toml"),
            "[panel]\nlayout = 'compact'\n",
        );

        for loaded in load_both(&options) {
            assert_eq!(loaded, expected);
        }
    }
}

#[test]
fn new_dropins_load_even_without_main() {
    let temp = tempfile::tempdir().expect("temp");
    let options = options(temp.path());
    let project = temp.path().join("project");
    write_file(&project.join("evorch.toml"), "[invalid TOML");
    write_file(&project.join("config.d/10-legacy.toml"), "[invalid TOML");
    write_file(
        &project.join(".evorch/config.d/10-new.toml"),
        "[metrics]\nretention_days = 9\n",
    );

    for loaded in load_both(&options) {
        assert_eq!(loaded.metrics.retention_days, 9);
    }
    assert!(!project_main_config_path(&project).exists());
}

#[test]
fn new_layout_does_not_merge_any_legacy_files() {
    let temp = tempfile::tempdir().expect("temp");
    let options = options(temp.path());
    let project = temp.path().join("project");
    write_file(
        &project.join(".evorch/config.toml"),
        "[metrics]\nretention_days = 7\n",
    );
    write_file(
        &project.join(".evorch/config.d/10-new.toml"),
        "[metrics]\nretention_days = 11\n",
    );
    write_file(
        &project.join("evorch.toml"),
        "[metrics]\nenabled = false\nretention_days = 99\n",
    );
    write_file(
        &project.join("config.d/90-legacy.toml"),
        "[panel]\nlayout = 'compact'\n",
    );

    for loaded in load_both(&options) {
        assert_eq!(loaded.metrics.retention_days, 11);
        assert_eq!(loaded.metrics.enabled, Config::default().metrics.enabled);
        assert_eq!(loaded.panel, Config::default().panel);
    }
}

#[test]
fn invalid_legacy_files_are_never_read() {
    for new_main in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let options = options(temp.path());
        let project = temp.path().join("project");
        if new_main {
            write_file(&project.join(".evorch/config.toml"), "");
        }
        write_file(&project.join("evorch.toml"), "[invalid TOML");
        write_file(&project.join("config.d/10-invalid.toml"), "[invalid TOML");

        for loaded in load_both(&options) {
            assert_eq!(loaded, Config::default());
        }
    }
}

#[test]
fn invalid_new_main_reports_its_path() {
    let temp = tempfile::tempdir().expect("temp");
    let options = options(temp.path());
    let project = temp.path().join("project");
    let new_main = project_main_config_path(&project);
    write_file(&new_main, "[invalid TOML");

    for result in [Config::load(&options), Config::load_strict(&options)] {
        assert!(matches!(result, Err(ConfigError::Parse { path, .. }) if path == new_main));
    }
}
