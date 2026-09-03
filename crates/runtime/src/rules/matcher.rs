//! スコープ付きルールの glob マッチング。

use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSetBuilder};

use super::types::{RuleMeta, RulesError};

pub(crate) fn matches(meta: &RuleMeta, rel_path: &str) -> Result<bool, RulesError> {
    if meta.always_apply {
        return Ok(true);
    }
    if meta.globs.is_empty() {
        return Ok(false);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in &meta.globs {
        let glob = Glob::new(pattern).map_err(|_| RulesError::InvalidGlob {
            path: PathBuf::from(rel_path),
        })?;
        builder.add(glob);
    }
    let set = builder.build().map_err(|_| RulesError::InvalidGlob {
        path: PathBuf::from(rel_path),
    })?;
    Ok(set.is_match(normalize_relative(rel_path)))
}

fn normalize_relative(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    Path::new(&normalized)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use crate::rules::types::RuleMeta;

    use super::matches;

    // Given: Rust ファイル向け glob / When: 一致・不一致パスを照合 / Then: 対応する結果になる
    #[test]
    fn matches_any_configured_glob() {
        let meta = RuleMeta {
            always_apply: false,
            globs: vec!["src/**/*.rs".to_string()],
        };

        assert!(matches(&meta, "src/rules/a.rs").expect("有効な glob"));
        assert!(!matches(&meta, "tests/a.rs").expect("有効な glob"));
    }

    // Given: 不正 glob / When: 照合 / Then: エラーになる
    #[test]
    fn invalid_glob_is_rejected() {
        let meta = RuleMeta {
            always_apply: false,
            globs: vec!["[".to_string()],
        };

        assert!(matches(&meta, "src/a.rs").is_err());
    }

    // Given: alwaysApply と不正 glob / When: 照合 / Then: glob を評価せず一致する
    #[test]
    fn always_apply_short_circuits_globs() {
        let meta = RuleMeta {
            always_apply: true,
            globs: vec!["[".to_string()],
        };

        assert!(matches(&meta, "src/a.rs").expect("alwaysApply が優先される"));
    }

    // Given: 重複区切りまたは先頭 dot を持つ相対パス / When: 照合 / Then: 正規化後に一致する
    #[test]
    fn normalizes_relative_path_separators() {
        let meta = RuleMeta {
            always_apply: false,
            globs: vec!["src/*.rs".to_string()],
        };

        assert!(matches(&meta, "src//a.rs").expect("有効な glob"));
        assert!(matches(&meta, "./src/a.rs").expect("有効な glob"));
    }
}
