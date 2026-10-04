//! プリセットストア (同梱プリセットとユーザー上書きの 2 層解決) を提供します。

use std::path::Path;

use crate::ConfigError;

/// プリセットファイルのサイズ上限 (64 KiB)。
const MAX_PRESET_BYTES: u64 = 64 * 1024;

/// 同梱プリセット (名前 → 本文)。include_str! によりバイナリへ埋め込む。
const BUNDLED: &[(&str, &str)] = &[
    (
        "role-webresearcher",
        include_str!("../assets/presets/role-webresearcher.md"),
    ),
    (
        "role-planner",
        include_str!("../assets/presets/role-planner.md"),
    ),
    (
        "role-oracle",
        include_str!("../assets/presets/role-oracle.md"),
    ),
    (
        "role-multimodallooker",
        include_str!("../assets/presets/role-multimodallooker.md"),
    ),
    (
        "role-orchestrator",
        include_str!("../assets/presets/role-orchestrator.md"),
    ),
    (
        "role-explorer",
        include_str!("../assets/presets/role-explorer.md"),
    ),
    (
        "role-worker",
        include_str!("../assets/presets/role-worker.md"),
    ),
    (
        "role-reviewer",
        include_str!("../assets/presets/role-reviewer.md"),
    ),
    (
        "family-claude",
        include_str!("../assets/presets/family-claude.md"),
    ),
    (
        "family-openai-reasoning",
        include_str!("../assets/presets/family-openai-reasoning.md"),
    ),
    (
        "family-gpt5",
        include_str!("../assets/presets/family-gpt5.md"),
    ),
    (
        "family-gemini",
        include_str!("../assets/presets/family-gemini.md"),
    ),
    (
        "family-kimi",
        include_str!("../assets/presets/family-kimi.md"),
    ),
    (
        "family-generic",
        include_str!("../assets/presets/family-generic.md"),
    ),
];

/// 同梱プリセットとユーザー上書きを 2 層で解決するストア。
pub struct PresetStore;

impl PresetStore {
    /// プリセット名から本文を解決する。
    ///
    /// `user_presets_dir` に `<name>.md` が存在すればそれを優先し、無ければ
    /// 同梱プリセットを返す。この関数は決してファイルを書き込まない。
    ///
    /// # Errors
    /// 名前が `[a-z0-9-]{1,64}` 外なら [`ConfigError::PresetNameInvalid`]、
    /// ユーザーファイルが 64 KiB 超なら [`ConfigError::PresetTooLarge`]、
    /// UTF-8 で読めなければ [`ConfigError::PresetNotUtf8`]、
    /// どちらにも存在しなければ [`ConfigError::PresetNotFound`] を返す。
    pub fn resolve(name: &str, user_presets_dir: Option<&Path>) -> Result<String, ConfigError> {
        validate_name(name)?;
        if let Some(dir) = user_presets_dir {
            let path = dir.join(format!("{name}.md"));
            if path.is_file() {
                return read_user_file(&path);
            }
        }
        bundled(name)
            .map(str::to_string)
            .ok_or_else(|| ConfigError::PresetNotFound {
                name: name.to_string(),
            })
    }
}

/// [`PresetStore::resolve`] が本文を読み込む層。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetOrigin {
    /// `<user_presets_dir>/<name>.md` のユーザー上書き。
    User(std::path::PathBuf),
    /// バイナリに同梱されたプリセット。
    Bundled,
}

impl PresetStore {
    /// [`PresetStore::resolve`] と同じ優先順位で、本文の解決元を返す。
    ///
    /// 名前が不正、またはどちらにも存在しなければ `None` を返す。
    pub fn origin(name: &str, user_presets_dir: Option<&Path>) -> Option<PresetOrigin> {
        validate_name(name).ok()?;
        if let Some(dir) = user_presets_dir {
            let path = dir.join(format!("{name}.md"));
            if path.is_file() {
                return Some(PresetOrigin::User(path));
            }
        }
        bundled(name).map(|_| PresetOrigin::Bundled)
    }
}

/// ユーザー上書きファイルを検査して読み込む。
fn read_user_file(path: &Path) -> Result<String, ConfigError> {
    let size = std::fs::metadata(path)?.len();
    if size > MAX_PRESET_BYTES {
        return Err(ConfigError::PresetTooLarge {
            path: path.to_path_buf(),
            size,
        });
    }
    std::fs::read_to_string(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::InvalidData {
            ConfigError::PresetNotUtf8 {
                path: path.to_path_buf(),
            }
        } else {
            ConfigError::Io(err)
        }
    })
}

/// プリセット名を `[a-z0-9-]{1,64}` に制限する。
///
/// 区切り文字 (`/`・`.` 等) を排除し、親ディレクトリ参照を不可能にする。
fn validate_name(name: &str) -> Result<(), ConfigError> {
    let is_valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if is_valid {
        return Ok(());
    }
    Err(ConfigError::PresetNameInvalid {
        name: name.to_string(),
    })
}

/// 同梱プリセットを名前で引く。
fn bundled(name: &str) -> Option<&'static str> {
    BUNDLED
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, body)| *body)
        .or_else(|| {
            crate::agent_categories::CATEGORIES
                .iter()
                .find(|category| category.overlay_preset == name)
                .map(|category| category.overlay_body)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 同梱 role / family / category プリセットの一覧 / When: ユーザー上書きなしで解決する
    // Then: すべて解決でき、本文が空でない
    #[test]
    fn bundled_store_contains_all_role_family_category_presets() {
        let bundled_names = [
            "role-orchestrator",
            "role-explorer",
            "role-worker",
            "role-reviewer",
            "family-claude",
            "family-openai-reasoning",
            "family-gpt5",
            "family-gemini",
            "family-kimi",
            "family-generic",
            "category-quick",
            "category-deep",
            "category-high-reasoning",
            "category-visual",
            "category-writing",
            "category-research",
            "category-plan-review",
            "category-tool-execution",
            "category-lesson",
            "category-lesson-review",
        ];

        for name in bundled_names {
            let body =
                PresetStore::resolve(name, None).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(!body.is_empty(), "{name} の本文は空でない");
        }
    }
}
