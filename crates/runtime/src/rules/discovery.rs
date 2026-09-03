//! 対象パスに適用可能なルールファイルの発見。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::path::{absolute_normalized, canonicalize, deepest_existing, slash_path};
use super::types::{RuleKind, RuleScope, RuleSourceFile, RulesError, ScopedDirKind};

pub(crate) fn chain_for_target(
    project_root: &Path,
    target: &Path,
) -> Result<Vec<RuleSourceFile>, RulesError> {
    let lexical_root = absolute_normalized(project_root)?;
    let lexical_target = if target.is_absolute() {
        absolute_normalized(target)?
    } else {
        absolute_normalized(&lexical_root.join(target))?
    };
    let start = if target.is_dir() {
        lexical_target
    } else {
        lexical_target
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| lexical_root.clone())
    };
    if !start.starts_with(&lexical_root) {
        return Err(RulesError::EscapedRoot { path: start });
    }
    let canonical_root = canonicalize(project_root)?;
    let existing = deepest_existing(&start);
    let canonical_start = canonicalize(&existing)?;
    if !canonical_start.starts_with(&canonical_root) {
        return Err(RulesError::EscapedRoot { path: start });
    }

    let mut levels = Vec::new();
    let mut current = start.as_path();
    loop {
        levels.push(current.to_path_buf());
        if current == lexical_root {
            break;
        }
        current = current.parent().ok_or_else(|| RulesError::EscapedRoot {
            path: start.clone(),
        })?;
    }
    levels.reverse();

    let mut sources = Vec::new();
    for (depth, directory) in levels.iter().enumerate() {
        collect_level(
            &canonical_root,
            &lexical_root,
            directory,
            u32::try_from(depth).unwrap_or(u32::MAX),
            &mut sources,
        )?;
    }
    Ok(sources)
}

pub(crate) fn union_targets(
    chains: impl IntoIterator<Item = Vec<RuleSourceFile>>,
) -> Vec<RuleSourceFile> {
    let mut seen = HashSet::new();
    let mut union: Vec<_> = chains
        .into_iter()
        .flatten()
        .filter(|source| seen.insert(source.canonical_path.clone()))
        .collect();
    union.sort_by(|left, right| {
        (left.depth, &left.rel_path, left.kind).cmp(&(right.depth, &right.rel_path, right.kind))
    });
    union
}

fn collect_level(
    canonical_root: &Path,
    lexical_root: &Path,
    directory: &Path,
    depth: u32,
    sources: &mut Vec<RuleSourceFile>,
) -> Result<(), RulesError> {
    let agents = directory.join("AGENTS.md");
    if agents.is_file() {
        sources.push(source_file(
            canonical_root,
            lexical_root,
            &agents,
            None,
            depth,
            RuleKind::AgentsMd,
        )?);
    }
    for dir_kind in ScopedDirKind::ALL {
        let scoped_dir = directory.join(dir_kind.dir_name());
        let mut paths = direct_rule_files(&scoped_dir)?;
        paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
        for path in paths {
            sources.push(source_file(
                canonical_root,
                lexical_root,
                &path,
                Some(dir_kind),
                depth,
                RuleKind::ScopedRule,
            )?);
        }
    }
    Ok(())
}

fn direct_rule_files(directory: &Path) -> Result<Vec<PathBuf>, RulesError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(RulesError::Io {
                path: directory.to_path_buf(),
                source,
            });
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| RulesError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let is_rule = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "md" | "mdc"));
        if is_rule && path.is_file() {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn source_file(
    canonical_root: &Path,
    lexical_root: &Path,
    path: &Path,
    dir_kind: Option<ScopedDirKind>,
    depth: u32,
    kind: RuleKind,
) -> Result<RuleSourceFile, RulesError> {
    let canonical_path = canonicalize(path)?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(RulesError::EscapedRoot {
            path: path.to_path_buf(),
        });
    }
    let relative = path
        .strip_prefix(lexical_root)
        .map_err(|_| RulesError::EscapedRoot {
            path: path.to_path_buf(),
        })?;
    Ok(RuleSourceFile {
        canonical_path,
        rel_path: slash_path(relative),
        dir_kind,
        depth,
        kind,
        scope: RuleScope::Project,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{chain_for_target, union_targets};

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("親がある")).expect("ディレクトリを作れる");
        std::fs::write(path, path.display().to_string()).expect("ファイルを書ける");
    }

    // Given: root と 2 階層下に AGENTS.md と scoped rules / When: 深い対象の chain を発見 / Then: root から深い順になる
    #[test]
    fn chain_is_root_to_deep_with_sorted_scoped_files() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let root = tmp.path();
        write(&root.join("AGENTS.md"));
        write(&root.join(".omo/rules/b.md"));
        write(&root.join(".omo/rules/a.mdc"));
        write(&root.join("src/AGENTS.md"));
        write(&root.join("src/deep/.cursor/rules/z.md"));
        write(&root.join("sibling/AGENTS.md"));
        let target = root.join("src/deep/new.rs");

        let chain = chain_for_target(root, &target).expect("chain を発見できる");
        let paths: Vec<_> = chain
            .iter()
            .map(|source| source.rel_path.as_str())
            .collect();

        assert_eq!(
            paths,
            [
                "AGENTS.md",
                ".omo/rules/a.mdc",
                ".omo/rules/b.md",
                "src/AGENTS.md",
                "src/deep/.cursor/rules/z.md"
            ]
        );
        assert!(!paths.iter().any(|path| path.contains("sibling")));
    }

    // Given: 対象が root 自身 / When: chain を発見 / Then: root の規則だけで親へ上がらない
    #[test]
    fn target_at_root_never_walks_above_root() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"));
        write(&tmp.path().parent().expect("親がある").join("AGENTS.md"));

        let chain = chain_for_target(tmp.path(), tmp.path()).expect("chain を発見できる");

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].rel_path, "AGENTS.md");
    }

    // Given: project root 相対の新規 target / When: chain を発見 / Then: project root 基準で解決される
    #[test]
    fn relative_target_is_resolved_from_project_root() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"));
        write(&tmp.path().join("src/AGENTS.md"));

        let chain =
            chain_for_target(tmp.path(), Path::new("src/new.rs")).expect("chain を発見できる");
        let paths: Vec<_> = chain
            .iter()
            .map(|source| source.rel_path.as_str())
            .collect();

        assert_eq!(paths, ["AGENTS.md", "src/AGENTS.md"]);
    }

    #[cfg(unix)]
    // Given: root 内から外部ディレクトリへ向く symlink / When: その配下を対象に発見 / Then: EscapedRoot になる
    #[test]
    fn symlink_escape_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root を作れる");
        let outside = tempfile::tempdir().expect("外部を作れる");
        symlink(outside.path(), root.path().join("escape")).expect("symlink を作れる");

        let result = chain_for_target(root.path(), &root.path().join("escape/new.rs"));

        assert!(matches!(
            result,
            Err(crate::rules::types::RulesError::EscapedRoot { .. })
        ));
    }

    // Given: 共有 root 規則を持つ 2 対象の chain / When: union / Then: 重複せず depth・path 順で安定する
    #[test]
    fn multi_target_union_is_stable_and_deduplicated() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let root = tmp.path();
        write(&root.join("AGENTS.md"));
        write(&root.join("a/AGENTS.md"));
        write(&root.join("b/AGENTS.md"));
        let a = chain_for_target(root, &root.join("a/new.rs")).expect("a chain");
        let b = chain_for_target(root, &root.join("b/new.rs")).expect("b chain");

        let union = union_targets([b, a]);
        let paths: Vec<_> = union
            .iter()
            .map(|source| source.rel_path.as_str())
            .collect();

        assert_eq!(paths, ["AGENTS.md", "a/AGENTS.md", "b/AGENTS.md"]);
    }
}
