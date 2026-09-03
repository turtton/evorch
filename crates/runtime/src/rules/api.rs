//! プロジェクトルールの公開スナップショット API。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use providers::Usage;

use super::budget::injection_budget_bytes;
use super::discovery::{chain_for_target, union_targets};
use super::frontmatter::{body_without_frontmatter, parse_frontmatter};
use super::matcher::matches;
use super::render::render_with_markers;
use super::session::RulesSession;
use super::source::RulesSource;
use super::types::{
    ProjectTrust, ResolvedRule, RuleKind, RuleMeta, RuleScope, RuleSourceFile, RulesError,
};

/// run 開始時に常時適用ルールのスナップショットを生成する。
pub fn startup_snapshot(
    source: &RulesSource,
    active_root: Option<&Path>,
    last_usage: Option<&Usage>,
    estimated_history_bytes: u64,
) -> Option<String> {
    let mut markers = Vec::new();
    let mut rules = user_startup_rules(source, &mut markers);
    if source.trust == ProjectTrust::Approved
        && let Some(root) = active_root
        && let Some(rule) = project_root_rule(root, &mut markers)
    {
        rules.push(rule);
    }
    render_snapshot(
        rules,
        markers,
        injection_budget_bytes(&source.settings, last_usage, estimated_history_bytes),
    )
}

/// 成功したツール呼び出しの対象パスに適用されるルールを再読して生成する。
pub fn after_successful_tools(session: &mut RulesSession, targets: &[PathBuf]) -> Option<String> {
    if session.source.trust != ProjectTrust::Approved || targets.is_empty() {
        return None;
    }
    let root = session.active_root.as_deref()?;
    let mut markers = Vec::new();
    let mut chains = Vec::new();
    let mut target_paths = Vec::new();
    for target in targets {
        match chain_for_target(root, target) {
            Ok(chain) => chains.push(chain),
            Err(error) => disable(&error, &mut markers),
        }
        if let Some(relative) = lexical_relative(root, target) {
            target_paths.push(relative);
        }
    }
    let sources = union_targets(chains);
    let mut resolved = Vec::new();
    for source in sources {
        match resolve_for_targets(source, &target_paths) {
            Ok(Some(rule)) => resolved.push(rule),
            Ok(None) => {}
            Err(error) => disable(&error, &mut markers),
        }
    }
    render_snapshot(
        resolved,
        markers,
        injection_budget_bytes(&session.source.settings, session.last_usage.as_ref(), 0),
    )
}

fn user_startup_rules(source: &RulesSource, markers: &mut Vec<String>) -> Vec<ResolvedRule> {
    let Some(directory) = source.user_rules_dir.as_deref() else {
        return Vec::new();
    };
    let paths = match direct_markdown_files(directory) {
        Ok(paths) => paths,
        Err(error) => {
            disable(&error, markers);
            return Vec::new();
        }
    };
    paths
        .into_iter()
        .filter_map(|path| match resolve_user_rule(directory, &path) {
            Ok(Some(rule)) => Some(rule),
            Ok(None) => None,
            Err(error) => {
                disable(&error, markers);
                None
            }
        })
        .collect()
}

fn project_root_rule(root: &Path, markers: &mut Vec<String>) -> Option<ResolvedRule> {
    let path = root.join("AGENTS.md");
    if !path.is_file() {
        return None;
    }
    match source_for_path(root, &path, RuleKind::AgentsMd, RuleScope::Project, 0)
        .and_then(resolve_agents)
    {
        Ok(rule) => Some(rule),
        Err(error) => {
            disable(&error, markers);
            None
        }
    }
}

fn resolve_user_rule(directory: &Path, path: &Path) -> Result<Option<ResolvedRule>, RulesError> {
    let source = source_for_path(directory, path, RuleKind::ScopedRule, RuleScope::User, 0)?;
    let raw = read_text(&source.canonical_path)?;
    let meta = parse_frontmatter(&source.canonical_path, &raw)?;
    if !meta.always_apply {
        return Ok(None);
    }
    Ok(Some(ResolvedRule {
        source,
        meta,
        body: body_without_frontmatter(&raw).to_string(),
    }))
}

fn resolve_for_targets(
    source: RuleSourceFile,
    targets: &[String],
) -> Result<Option<ResolvedRule>, RulesError> {
    if source.kind == RuleKind::AgentsMd {
        return resolve_agents(source).map(Some);
    }
    let raw = read_text(&source.canonical_path)?;
    let meta = parse_frontmatter(&source.canonical_path, &raw)?;
    let mut selected = false;
    for target in targets {
        if matches(&meta, target).map_err(|error| match error {
            RulesError::InvalidGlob { .. } => RulesError::InvalidGlob {
                path: source.canonical_path.clone(),
            },
            other => other,
        })? {
            selected = true;
            break;
        }
    }
    Ok(selected.then(|| ResolvedRule {
        source,
        meta,
        body: body_without_frontmatter(&raw).to_string(),
    }))
}

fn resolve_agents(source: RuleSourceFile) -> Result<ResolvedRule, RulesError> {
    let body = read_text(&source.canonical_path)?;
    Ok(ResolvedRule {
        source,
        meta: RuleMeta {
            always_apply: true,
            globs: Vec::new(),
        },
        body,
    })
}

fn direct_markdown_files(directory: &Path) -> Result<Vec<PathBuf>, RulesError> {
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
        if path.extension().is_some_and(|extension| extension == "md") && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(paths)
}

fn source_for_path(
    root: &Path,
    path: &Path,
    kind: RuleKind,
    scope: RuleScope,
    depth: u32,
) -> Result<RuleSourceFile, RulesError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|source| RulesError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let canonical_path = std::fs::canonicalize(path).map_err(|source| RulesError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(RulesError::EscapedRoot {
            path: path.to_path_buf(),
        });
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| RulesError::EscapedRoot {
            path: path.to_path_buf(),
        })?;
    Ok(RuleSourceFile {
        canonical_path,
        rel_path: relative.to_string_lossy().replace('\\', "/"),
        dir_kind: None,
        depth,
        kind,
        scope,
    })
}

fn read_text(path: &Path) -> Result<String, RulesError> {
    std::fs::read_to_string(path).map_err(|source| RulesError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn lexical_relative(root: &Path, target: &Path) -> Option<String> {
    let absolute = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.join(target)
    };
    absolute
        .strip_prefix(root)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn disable(error: &RulesError, markers: &mut Vec<String>) {
    tracing::warn!(error = %error, "project rule source disabled");
    markers.push(format!("- [rules disabled: {error}]"));
}

fn render_snapshot(
    rules: Vec<ResolvedRule>,
    mut markers: Vec<String>,
    budget: u64,
) -> Option<String> {
    if rules.is_empty() && markers.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    markers.retain(|marker| seen.insert(marker.clone()));
    Some(render_with_markers(rules, budget, &markers))
}
