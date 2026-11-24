//! `@` mentions: project file/skill index, cursor token detection and skill expansion.
//!
//! The file index and token detection are shared with the core client
//! ([`crate::composer_input::mentions`]); this adds the runtime's skills.

#[cfg(test)]
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use runtime::skill::SkillRegistry;

use crate::composer_input::mentions::MAX_INDEXED_PATHS;
pub use crate::composer_input::mentions::{
    CompletionItem, CompletionKind, MentionIndex, MentionQuery, mention_at,
};
#[cfg(test)]
use crate::composer_input::mentions::{GIT_LOCAL_ENV, MAX_COMPLETIONS, slash_path};

impl MentionIndex {
    /// Indexes `root`, then each of `allowed` within the remaining path budget.
    /// `skills` are what a mention in `root` reaches, which include its
    /// repository skills when `repo_skills` (see [`mention_skills`]).
    pub fn build(
        root: &Path,
        allowed: &[PathBuf],
        skills: &SkillRegistry,
        repo_skills: bool,
    ) -> Self {
        Self::build_within(root, allowed, skills, repo_skills, MAX_INDEXED_PATHS)
    }

    /// [`Self::build`] with at most `limit` files across all directories.
    pub(crate) fn build_within(
        root: &Path,
        allowed: &[PathBuf],
        skills: &SkillRegistry,
        repo_skills: bool,
        limit: usize,
    ) -> Self {
        let skills = skills
            .available_skills()
            .into_iter()
            .map(|skill| (skill.name, skill.description));
        Self::index_within(root, allowed, skills, repo_skills, limit)
    }
}

/// Names written as `@name` at word boundaries, deduplicated in first-seen order.
fn mentioned_names(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    for token in text.split_whitespace() {
        let Some(name) = token.strip_prefix('@') else {
            continue;
        };
        let name = name.trim_end_matches(|c: char| ".,;:!?)]}\"'".contains(c));
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The skills an `@name` mention in `root` reaches: those the runtime gives
/// the project's runs, so its repository skills only when its trust admits
/// them (`repo_skills`, see `project_bridge::repo_skills_admitted`).
pub fn mention_skills(root: Option<&Path>, repo_skills: bool) -> SkillRegistry {
    runtime::skill::build_standard_registry(root.filter(|_| repo_skills))
}

/// Whether `@name` inlines one of `skills`. A project path with the same name
/// wins, keeping `@docs` a folder reference.
fn inlines(name: &str, root: Option<&Path>, skills: &SkillRegistry) -> bool {
    skills.get(name).is_some() && !root.is_some_and(|root| root.join(name).exists())
}

/// The skills an `@name` mention in `text` attaches, once each in first-seen
/// order. The runtime reads their bodies when the message is sent, so edits
/// apply on that send and an unreadable skill refuses it.
///
/// A project path with the same name wins, keeping `@docs` a folder reference.
pub fn mentioned_skills(text: &str, root: Option<&Path>, skills: &SkillRegistry) -> Vec<String> {
    mentioned_names(text)
        .into_iter()
        .filter(|name| inlines(name, root, skills))
        .map(str::to_owned)
        .collect()
}

/// The mentioned names, once each in first-seen order, that
/// [`mentioned_skills`] leaves as text with `skills` yet would inline
/// with `trusted`, the skills the same project reaches once trusted: its
/// repository skills that its trust withholds.
pub fn withheld_skill_mentions<'a>(
    text: &'a str,
    root: Option<&Path>,
    skills: &SkillRegistry,
    trusted: &SkillRegistry,
) -> Vec<&'a str> {
    mentioned_names(text)
        .into_iter()
        .filter(|name| !inlines(name, root, skills) && inlines(name, root, trusted))
        .collect()
}

#[cfg(test)]
#[path = "composer_mention_tests.rs"]
mod tests;
