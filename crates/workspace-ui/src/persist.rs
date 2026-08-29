use std::fs;
use std::path::Path;

use crate::{PersistError, SettingsError, UI_SETTINGS_VERSION, UiSettings, Workspace, validate};

/// Workspace を検証して pretty JSON に直列化します。
///
/// # Errors
/// レイアウトが不正、または JSON 直列化に失敗した場合に返します。
pub fn to_json(workspace: &Workspace) -> Result<String, PersistError> {
    validate(workspace)?;
    serde_json::to_string_pretty(workspace)
        .map_err(|error| PersistError::Serialization(error.to_string()))
}

/// JSON を Workspace として解析し、全不変条件を検証します。
///
/// # Errors
/// JSON の解析またはレイアウト検証に失敗した場合に返します。
pub fn from_json(json: &str) -> Result<Workspace, PersistError> {
    let workspace = serde_json::from_str(json)
        .map_err(|error| PersistError::Serialization(error.to_string()))?;
    validate(&workspace)?;
    Ok(workspace)
}

/// Workspace JSON を指定パスへ保存します。
///
/// # Errors
/// レイアウト検証、JSON 直列化、ファイル書き込みの失敗時に返します。
pub fn save_to(workspace: &Workspace, path: &Path) -> Result<(), PersistError> {
    let json = to_json(workspace)?;
    fs::write(path, json).map_err(|error| PersistError::Io(error.to_string()))
}

/// 指定パスから Workspace JSON を読み込みます。
///
/// # Errors
/// ファイル読み込み、JSON 解析、レイアウト検証の失敗時に返します。
pub fn load_from(path: &Path) -> Result<Workspace, PersistError> {
    let json = fs::read_to_string(path).map_err(|error| PersistError::Io(error.to_string()))?;
    from_json(&json)
}

pub use load_from as load_workspace;
pub use save_to as save_workspace;

/// UI 設定を TOML として保存します。
///
/// # Errors
/// 埋め込み Workspace が不正、TOML 直列化、または書き込みに失敗した場合に返します。
pub fn save_settings(settings: &UiSettings, path: &Path) -> Result<(), SettingsError> {
    validate_settings(settings)?;
    let document = toml::to_string_pretty(settings)
        .map_err(|error| SettingsError::Serialization(error.to_string()))?;
    fs::write(path, document).map_err(|error| SettingsError::Io(error.to_string()))
}

/// 指定パスから TOML UI 設定を読み込みます。
///
/// # Errors
/// 読み込み、TOML 解析、バージョンまたはレイアウト検証の失敗時に返します。
pub fn load_settings(path: &Path) -> Result<UiSettings, SettingsError> {
    let document =
        fs::read_to_string(path).map_err(|error| SettingsError::Io(error.to_string()))?;
    let settings = toml::from_str(&document)
        .map_err(|error| SettingsError::Serialization(error.to_string()))?;
    validate_settings(&settings)?;
    Ok(settings)
}

fn validate_settings(settings: &UiSettings) -> Result<(), SettingsError> {
    if settings.version != UI_SETTINGS_VERSION {
        return Err(SettingsError::UnsupportedVersion {
            found: settings.version,
            supported: UI_SETTINGS_VERSION,
        });
    }
    if let Some(workspace) = &settings.layout.workspace {
        validate(workspace)?;
    }
    Ok(())
}
