use std::str::FromStr;

use tempfile::tempdir;
use workspace_ui::{
    KeyAction, KeyChord, LayoutNode, PanelKind, SettingsError, UiSettings, load_settings,
    save_settings,
};

#[test]
fn public_settings_toml_contains_layout_panel_kinds_and_keybinds() {
    // Given: settings embedding the default agent/terminal/tasks workspace.
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace_ui::Workspace::default_v01());
    let directory = tempdir().expect("temporary directory must be created");
    let path = directory.path().join("ui.toml");

    // When: settings are saved using the public TOML format and loaded again.
    save_settings(&settings, &path).expect("settings must save");
    let document = std::fs::read_to_string(&path).expect("saved settings must be readable");
    let restored = load_settings(&path).expect("saved settings must load");

    // Then: the public document exposes all pane kinds and keybind configuration.
    let parsed: toml::Value = toml::from_str(&document).expect("settings must be valid TOML");
    let serialized = parsed.to_string();
    assert!(serialized.contains("agent"));
    assert!(serialized.contains("terminal"));
    assert!(serialized.contains("tasks"));
    assert!(parsed.get("keybinds").is_some());
    assert_eq!(restored, settings);
    assert_eq!(
        restored.layout.workspace.as_ref().map(|workspace| {
            workspace
                .panels
                .values()
                .map(|panel| panel.kind)
                .collect::<Vec<PanelKind>>()
        }),
        settings.layout.workspace.as_ref().map(|workspace| {
            workspace
                .panels
                .values()
                .map(|panel| panel.kind)
                .collect::<Vec<PanelKind>>()
        })
    );
}

#[test]
fn missing_sections_fall_back_to_defaults() {
    // Given: a minimal v0.1 settings document.
    let source = "version = 1\n";

    // When: serde parses omitted layout and keybind sections.
    let parsed: UiSettings = toml::from_str(source).expect("minimal settings must parse");

    // Then: backward-compatible defaults fill both sections.
    assert_eq!(parsed, UiSettings::default());
}

#[test]
fn keychord_roundtrips_and_rejects_invalid_syntax() {
    // Given: modifier combinations and malformed chord strings.
    let chords = ["Ctrl+1", "Ctrl+Shift+S", "Alt+Enter"];

    // When: each valid chord is parsed, displayed, and parsed again.
    for source in chords {
        let parsed = KeyChord::from_str(source).expect("valid chord must parse");
        let restored = KeyChord::from_str(&parsed.to_string()).expect("display must parse");
        // Then: display is a stable serde representation.
        assert_eq!(restored, parsed);
    }

    assert_eq!(
        KeyChord::from_str("Ctrl+"),
        Err(SettingsError::InvalidKeyChord("Ctrl+".to_owned()))
    );
    assert_eq!(
        KeyChord::from_str("Meta+S"),
        Err(SettingsError::InvalidKeyChord("Meta+S".to_owned()))
    );
}

#[test]
fn default_keybinds_cover_every_action() {
    // Given: the public v0.1 defaults.
    let settings = UiSettings::default();

    // When: all supported actions are queried.
    let actions = [
        KeyAction::FocusAgentPane,
        KeyAction::FocusTerminalPane,
        KeyAction::FocusTasksPane,
        KeyAction::SaveLayout,
        KeyAction::ResetLayout,
    ];

    // Then: every action has exactly one binding.
    assert_eq!(settings.keybinds.bindings.len(), actions.len());
    for action in actions {
        assert!(settings.keybinds.bindings.contains_key(&action));
    }
}

#[test]
fn settings_load_rejects_unsupported_version_and_invalid_workspace() {
    // Given: unsupported settings and a settings file embedding an invalid workspace.
    let directory = tempdir().expect("temporary directory must be created");
    let unsupported_path = directory.path().join("unsupported.toml");
    std::fs::write(&unsupported_path, "version = 99\n")
        .expect("unsupported fixture must be writable");

    let mut invalid = UiSettings::default();
    let mut workspace = workspace_ui::Workspace::default_v01();
    let LayoutNode::Split(split) = &mut workspace.main.root else {
        panic!("default root must be a split");
    };
    split.fraction = 1.0;
    invalid.layout.workspace = Some(workspace);
    let invalid_path = directory.path().join("invalid.toml");
    let invalid_toml = toml::to_string(&invalid).expect("invalid fixture must serialize");
    std::fs::write(&invalid_path, invalid_toml).expect("invalid fixture must be writable");

    // When: both documents cross the settings load boundary.
    let unsupported = load_settings(&unsupported_path);
    let invalid_layout = load_settings(&invalid_path);

    // Then: version and embedded layout failures remain typed and distinguishable.
    assert_eq!(
        unsupported,
        Err(SettingsError::UnsupportedVersion {
            found: 99,
            supported: 1,
        })
    );
    assert!(matches!(
        invalid_layout,
        Err(SettingsError::Layout(workspace_ui::LayoutError::InvalidFraction { fraction }))
            if fraction == 1.0
    ));
}
