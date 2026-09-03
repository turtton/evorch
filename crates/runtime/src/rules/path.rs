//! ルール探索で使う lexical path と canonical path の処理。

use std::path::{Component, Path, PathBuf};

use super::types::RulesError;

pub(super) fn absolute_normalized(path: &Path) -> Result<PathBuf, RulesError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| RulesError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

pub(super) fn deepest_existing(path: &Path) -> PathBuf {
    let mut current = path;
    while !current.exists() {
        let Some(parent) = current.parent() else {
            return current.to_path_buf();
        };
        current = parent;
    }
    current.to_path_buf()
}

pub(super) fn canonicalize(path: &Path) -> Result<PathBuf, RulesError> {
    std::fs::canonicalize(path).map_err(|source| RulesError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_string()),
            Component::Prefix(_) | Component::RootDir | Component::CurDir => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: dot・parent・重複区切りを含む絶対パス / When: lexical 正規化 / Then: 意味上同じ絶対パスになる
    #[test]
    fn normalizes_lexical_components() {
        let normalized =
            absolute_normalized(Path::new("/tmp/a/./b/../c//d")).expect("正規化できる");

        assert_eq!(normalized, PathBuf::from("/tmp/a/c/d"));
    }

    // Given: OS path / When: slash path 化 / Then: 相対 component が slash 区切りになる
    #[test]
    fn slash_path_joins_relative_components() {
        assert_eq!(slash_path(Path::new("src/rules/a.rs")), "src/rules/a.rs");
    }
}
