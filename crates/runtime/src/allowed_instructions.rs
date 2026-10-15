//! Rules and repository skills of allowed directories, read when a run first
//! touches one (ADR 0032). They are appended after the tool round that touched
//! the directory; the system message and the shared skill catalog never change.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agent_loop::LoopState;
use crate::rules;
use crate::skill::{SkillEntry, SkillRegistry, SkillSource, discover_skills, repo_skill_dirs};

const HEADER: &str = "[allowed-directory instructions]";

/// What a run has read from its allowed directories, saved in its restore snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedInstructions {
    /// Directories whose instructions were appended since history was last replaced.
    #[serde(default)]
    pub touched: BTreeSet<PathBuf>,
    /// Rule files appended since history was last replaced.
    #[serde(default)]
    pub injected_rules: BTreeSet<PathBuf>,
    /// Directories whose skills extend this run's registry.
    #[serde(default)]
    pub skill_roots: BTreeSet<PathBuf>,
}

impl AllowedInstructions {
    pub fn is_empty(&self) -> bool {
        self.touched.is_empty() && self.injected_rules.is_empty() && self.skill_roots.is_empty()
    }

    /// Compaction or a fork replaced history, which may have dropped the appended
    /// text, so the next touch appends it again. Loaded skills stay loadable.
    pub(crate) fn forget_history(&mut self) {
        self.touched.clear();
        self.injected_rules.clear();
    }
}

/// `target` made absolute against `base`, with its deepest existing ancestor
/// canonicalized so a symlink cannot place it inside or outside a directory.
fn resolve(target: &Path, base: Option<&Path>) -> Option<PathBuf> {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        base?.join(target)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(existing.file_name()?);
        existing = existing.parent()?;
    }
    let mut resolved = existing.canonicalize().ok()?;
    resolved.extend(missing.iter().rev());
    Some(resolved)
}

/// The allowed directory holding `target`. Targets in the project root keep
/// the project's own rules, even when an allowed directory contains the root.
fn allowed_root<'a>(
    target: &Path,
    project_root: Option<&Path>,
    allowed: &'a [PathBuf],
) -> Option<&'a PathBuf> {
    if project_root.is_some_and(|root| target.starts_with(root)) {
        return None;
    }
    allowed
        .iter()
        .find(|directory| target.starts_with(directory))
}

/// Repository skills of `root` whose files really live inside it.
fn directory_skills(root: &Path) -> Vec<SkillEntry> {
    let inside = |path: &Path| {
        path.canonicalize()
            .is_ok_and(|canonical| canonical.starts_with(root))
    };
    let dirs: Vec<_> = repo_skill_dirs(root)
        .into_iter()
        .filter(|(_, dir)| inside(dir))
        .collect();
    discover_skills(&dirs)
        .entries()
        .filter(|entry| {
            matches!(&entry.source, SkillSource::Filesystem { dir } if inside(&dir.join("SKILL.md")))
        })
        .cloned()
        .collect()
}

fn skills_listing(loadable: &[SkillEntry], shadowed: &[SkillEntry]) -> Option<String> {
    let line = |entry: &SkillEntry, note: &str| {
        tools::escape_control_markers(&format!("- {}: {}{note}", entry.name, entry.description))
    };
    let lines: Vec<_> = loadable
        .iter()
        .map(|entry| line(entry, ""))
        .chain(shadowed.iter().map(|entry| {
            line(
                entry,
                " (not loadable: another skill already uses this name)",
            )
        }))
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "Repository skills here (load with skill_load, or pass to delegate load_skills):\n{}",
            lines.join("\n")
        )
    })
}

/// Rule targets of one tool round, split by where their rules come from.
#[derive(Debug, Default)]
pub(crate) struct RoutedTargets {
    pub(crate) project: Vec<PathBuf>,
    pub(crate) allowed: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl LoopState {
    fn allowed_list(&self) -> Vec<PathBuf> {
        self.allowed_directories
            .as_ref()
            .map(|regions| regions.paths.get().to_vec())
            .unwrap_or_default()
    }

    /// Routes file-tool paths and shell `cwd`s of successful calls. Paths outside
    /// every allowed directory keep the project route; such `cwd`s are dropped.
    pub(crate) fn route_rule_targets(
        &self,
        file_targets: Vec<PathBuf>,
        shell_cwds: Vec<PathBuf>,
    ) -> RoutedTargets {
        let mut routed = RoutedTargets::default();
        let allowed = self.allowed_list();
        if allowed.is_empty() {
            routed.project = file_targets;
            return routed;
        }
        let base = self
            .rules_session
            .as_ref()
            .and_then(|session| session.active_root.clone())
            .or_else(|| self.shared.executor.default_cwd());
        let project_root = base.as_deref().and_then(|root| root.canonicalize().ok());
        let mut route = |target: &Path| {
            let resolved = resolve(target, base.as_deref())?;
            let root = allowed_root(&resolved, project_root.as_deref(), &allowed)?;
            routed
                .allowed
                .entry(root.clone())
                .or_default()
                .push(resolved);
            Some(())
        };
        let mut project = Vec::new();
        for target in file_targets {
            if route(&target).is_none() {
                project.push(target);
            }
        }
        for cwd in shell_cwds {
            route(&cwd);
        }
        routed.project = project;
        routed
    }

    /// Appends, as one message, the not yet appended rules of each touched allowed
    /// directory and, on its first touch, its repository skills. The record changes
    /// together with the history, so a snapshot never disagrees with what was sent.
    pub(crate) fn append_allowed_instructions(&mut self, touched: BTreeMap<PathBuf, Vec<PathBuf>>) {
        let mut budget = self.rules_session.as_ref().map(rules::injection_budget);
        let mut blocks = Vec::new();
        for (root, targets) in touched {
            let header = format!("{HEADER} {}", root.display());
            let mut parts = Vec::new();
            if let Some(remaining) = budget.as_mut() {
                let (text, included) = rules::allowed_directory_rules(
                    &root,
                    &targets,
                    &self.allowed_instructions.injected_rules,
                    &header,
                    *remaining,
                );
                if let Some(text) = text {
                    *remaining =
                        remaining.saturating_sub(u64::try_from(text.len()).unwrap_or(u64::MAX));
                    parts.push(text);
                }
                self.allowed_instructions.injected_rules.extend(included);
            }
            if self.allowed_instructions.touched.insert(root.clone()) {
                let (loadable, shadowed) = self.extend_skills(&root);
                parts.extend(skills_listing(&loadable, &shadowed));
            }
            if parts.is_empty() {
                continue;
            }
            let block = parts.join("\n");
            blocks.push(if block.starts_with(&header) {
                block
            } else {
                format!("{header}\n{block}")
            });
        }
        if !blocks.is_empty() {
            self.context.push_user(&blocks.join("\n\n"));
            self.publish_message_count();
        }
    }

    /// Adds the skills of `root` to this run's own registry; the shared catalog is
    /// untouched. A name already taken by another skill keeps that skill.
    fn extend_skills(&mut self, root: &Path) -> (Vec<SkillEntry>, Vec<SkillEntry>) {
        let mut registry = self
            .shared
            .skills
            .as_deref()
            .cloned()
            .unwrap_or_else(|| SkillRegistry::new(BTreeMap::new(), Vec::new()));
        let (mut loadable, mut shadowed) = (Vec::new(), Vec::new());
        for entry in directory_skills(root) {
            if registry.get(&entry.name).is_none() {
                self.allowed_skill_origins
                    .insert(entry.name.clone(), root.to_path_buf());
                registry.merge_shadowing(vec![entry.clone()]);
                loadable.push(entry);
            } else if self
                .allowed_skill_origins
                .get(&entry.name)
                .map(PathBuf::as_path)
                == Some(root)
            {
                loadable.push(entry);
            } else {
                shadowed.push(entry);
            }
        }
        self.allowed_instructions
            .skill_roots
            .insert(root.to_path_buf());
        self.shared.skills = Some(Arc::new(registry));
        (loadable, shadowed)
    }

    /// Makes the skills of `roots` that are still allowed loadable again, for a
    /// restored run or a child given them through `load_skills`. Nothing is appended.
    pub(crate) fn restore_allowed_skills(&mut self, roots: impl IntoIterator<Item = PathBuf>) {
        let allowed = self.allowed_list();
        for root in roots {
            if allowed.contains(&root) {
                self.extend_skills(&root);
            }
        }
    }

    /// Why `name` may not be loaded: its allowed directory was removed or untrusted.
    pub(crate) fn revoked_skill(&self, name: &str) -> Option<String> {
        let root = self.allowed_skill_origins.get(name)?;
        (!self.allowed_list().contains(root)).then(|| {
            format!(
                "skill {name} is unavailable: {} is no longer an allowed directory",
                root.display()
            )
        })
    }

    pub(crate) fn allowed_skill_roots(&self, names: &[String]) -> Vec<PathBuf> {
        names
            .iter()
            .filter_map(|name| self.allowed_skill_origins.get(name).cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn skill(root: &Path, scope_dir: &str, name: &str) {
        write(
            &root.join(scope_dir).join(name).join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: {name} steps\n---\n{name} body\n"),
        );
    }

    #[test]
    fn targets_resolve_into_allowed_directories_but_never_the_project_or_outside() {
        // Given: a project root inside an allowed directory, and an unrelated directory
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let allowed = base.join("allowed");
        let project = allowed.join("project");
        let outside = base.join("outside");
        for dir in [&project, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let list = vec![allowed.clone()];
        let route = |target: &Path| {
            resolve(target, Some(&project))
                .and_then(|resolved| allowed_root(&resolved, Some(&project), &list).cloned())
        };
        // When / Then: new files and relative paths resolve; the project and outside stay out
        assert_eq!(route(&allowed.join("lib/new.rs")), Some(allowed.clone()));
        assert_eq!(route(Path::new("../notes.md")), Some(allowed.clone()));
        assert_eq!(route(&project.join("src/main.rs")), None);
        assert_eq!(route(&outside.join("x.rs")), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_judged_by_where_they_lead() {
        // Given: a link inside the allowed directory to outside, and one from outside into it
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let allowed = base.join("allowed");
        let outside = base.join("outside");
        for dir in [&allowed, &outside] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        std::os::unix::fs::symlink(&allowed, outside.join("into")).unwrap();
        let list = vec![allowed.clone()];
        let route = |target: &Path| {
            resolve(target, None).and_then(|resolved| allowed_root(&resolved, None, &list).cloned())
        };
        // When / Then
        assert_eq!(route(&allowed.join("escape/x.rs")), None);
        assert_eq!(route(&outside.join("into/x.rs")), Some(allowed));
    }

    #[cfg(unix)]
    #[test]
    fn skills_must_live_inside_their_allowed_directory() {
        // Given: one real skill, and a skill directory linked from outside
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let allowed = base.join("allowed");
        skill(&allowed, ".evorch/skills", "local");
        skill(&base.join("outside"), "skills", "foreign");
        std::fs::create_dir_all(allowed.join(".agents")).unwrap();
        std::os::unix::fs::symlink(base.join("outside/skills"), allowed.join(".agents/skills"))
            .unwrap();
        // When
        let names: Vec<_> = directory_skills(&allowed)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        // Then
        assert_eq!(names, ["local"]);
    }

    #[test]
    fn listing_names_loadable_and_shadowed_skills() {
        let entry = |name: &str| SkillEntry {
            name: name.into(),
            description: format!("{name} steps"),
            source: SkillSource::Filesystem { dir: name.into() },
            scope: crate::skill::SkillScope::Repo,
        };
        assert_eq!(skills_listing(&[], &[]), None);
        assert_eq!(
            skills_listing(&[entry("deploy")], &[entry("git")]).unwrap(),
            "Repository skills here (load with skill_load, or pass to delegate load_skills):\n- deploy: deploy steps\n- git: git steps (not loadable: another skill already uses this name)"
        );
    }

    #[test]
    fn forgetting_history_keeps_loaded_skills() {
        let mut record = AllowedInstructions {
            touched: [PathBuf::from("/a")].into(),
            injected_rules: [PathBuf::from("/a/AGENTS.md")].into(),
            skill_roots: [PathBuf::from("/a")].into(),
        };
        record.forget_history();
        assert!(record.touched.is_empty() && record.injected_rules.is_empty());
        assert_eq!(record.skill_roots, [PathBuf::from("/a")].into());
        assert!(!record.is_empty());
    }
}
