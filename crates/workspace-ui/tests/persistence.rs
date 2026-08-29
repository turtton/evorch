use std::fs;

use tempfile::tempdir;
use workspace_ui::{
    LayoutError, PersistError, Workspace, from_json, load_workspace, save_workspace, to_json,
};

#[test]
fn workspace_json_and_file_roundtrips_preserve_layout() {
    // Given: the valid default workspace and an isolated path.
    let workspace = Workspace::default_v01();
    let directory = tempdir().expect("temporary directory must be created");
    let path = directory.path().join("workspace.json");

    // When: the model is round-tripped through JSON and the file API.
    let json = to_json(&workspace).expect("valid workspace must serialize");
    let from_memory = from_json(&json).expect("serialized workspace must load");
    save_workspace(&workspace, &path).expect("valid workspace must save");
    let from_file = load_workspace(&path).expect("saved workspace must load");

    // Then: both public persistence surfaces preserve the complete model.
    assert_eq!(from_memory, workspace);
    assert_eq!(from_file, workspace);
}

#[test]
fn load_rejects_unsupported_and_invalid_layouts() {
    // Given: serialized workspaces with an unsupported version and an invalid fraction.
    let mut unsupported = Workspace::default_v01();
    unsupported.version = 99;
    let unsupported_json =
        serde_json::to_string(&unsupported).expect("fixture serialization must succeed");

    let mut invalid = Workspace::default_v01();
    let workspace_ui::LayoutNode::Split(split) = &mut invalid.main.root else {
        panic!("default root must be a split");
    };
    split.fraction = 0.0;
    let invalid_json = serde_json::to_string(&invalid).expect("fixture serialization must succeed");

    // When: both payloads cross the load boundary.
    let unsupported_result = from_json(&unsupported_json);
    let invalid_result = from_json(&invalid_json);

    // Then: validation errors are preserved by the persistence error type.
    assert_eq!(
        unsupported_result,
        Err(PersistError::Layout(LayoutError::UnsupportedVersion {
            found: 99,
            supported: 1,
        }))
    );
    assert!(matches!(
        invalid_result,
        Err(PersistError::Layout(LayoutError::InvalidFraction { fraction })) if fraction == 0.0
    ));
}

#[test]
fn load_reports_malformed_json_and_missing_files() {
    // Given: malformed JSON and a path that does not exist.
    let directory = tempdir().expect("temporary directory must be created");
    let path = directory.path().join("missing.json");

    // When: each invalid source is loaded.
    let malformed = from_json("{");
    let missing = load_workspace(&path);

    // Then: serialization and I/O failures remain distinguishable.
    assert!(matches!(malformed, Err(PersistError::Serialization(_))));
    assert!(matches!(missing, Err(PersistError::Io(_))));

    fs::write(directory.path().join("bad.json"), "{").expect("fixture must be writable");
}
