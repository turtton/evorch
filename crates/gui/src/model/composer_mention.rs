//! `@` mentions: project file/skill index, cursor token detection and skill expansion.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use runtime::skill::SkillRegistry;

/// Upper bound on indexed paths so huge checkouts keep matching per keystroke cheap.
pub const MAX_INDEXED_PATHS: usize = 50_000;
pub const MAX_COMPLETIONS: usize = 8;
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];
/// `git rev-parse --local-env-vars`: an inherited hook environment must not redirect the listing.
const GIT_LOCAL_ENV: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];
const SKILL_BLOCK_START: &str = "\n\n<skill name=\"";
const SKILL_BLOCK_END: &str = "\n</skill>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    File,
    Dir,
    Skill,
}

/// One candidate: accepting it replaces `range` of the draft with `replacement`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub kind: CompletionKind,
    pub label: String,
    pub detail: String,
    pub range: Range<usize>,
    pub replacement: String,
}

impl CompletionItem {
    /// False when the draft already spells this candidate, so Enter can send instead.
    pub fn changes(&self, input: &str) -> bool {
        input.get(self.range.clone()) != Some(self.replacement.trim_end())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionEntry {
    pub kind: CompletionKind,
    /// Repo-relative path with `/` separators, or the skill name.
    pub value: String,
    pub detail: String,
    key: String,
}

impl MentionEntry {
    fn new(kind: CompletionKind, value: String, detail: String) -> Self {
        Self {
            kind,
            key: value.to_lowercase(),
            value,
            detail,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MentionIndex {
    pub root: Option<PathBuf>,
    pub entries: Vec<MentionEntry>,
}

/// The `@token` under the cursor; `range` covers the whole token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionQuery<'a> {
    pub query: &'a str,
    pub range: Range<usize>,
}

impl MentionIndex {
    pub fn build(root: &Path, skills: &SkillRegistry) -> Self {
        let skills = skills
            .available_skills()
            .into_iter()
            .map(|skill| (skill.name, skill.description));
        Self::from_parts(Some(root.to_owned()), &list_files(root), skills)
    }

    /// Folders are derived from file paths; `skills` are `(name, description)`.
    pub fn from_parts(
        root: Option<PathBuf>,
        files: &[String],
        skills: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let dirs: BTreeSet<&str> = files
            .iter()
            .flat_map(|file| {
                file.match_indices('/')
                    .map(move |(index, _)| &file[..index])
            })
            .collect();
        let mut entries: Vec<_> = dirs
            .into_iter()
            .map(|dir| MentionEntry::new(CompletionKind::Dir, dir.to_owned(), "folder".into()))
            .collect();
        entries.extend(
            files
                .iter()
                .map(|file| MentionEntry::new(CompletionKind::File, file.clone(), "file".into())),
        );
        entries.extend(skills.into_iter().map(|(name, description)| {
            MentionEntry::new(
                CompletionKind::Skill,
                name,
                format!("skill · {description}"),
            )
        }));
        Self { root, entries }
    }

    pub fn complete(&self, mention: &MentionQuery<'_>) -> Vec<CompletionItem> {
        let query = mention.query.to_lowercase();
        let mut scored: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| score(&entry.key, &query).map(|score| (score, entry)))
            .collect();
        scored.sort_by(|(left, a), (right, b)| left.cmp(right).then_with(|| a.key.cmp(&b.key)));
        scored
            .into_iter()
            .take(MAX_COMPLETIONS)
            .map(|(_, entry)| {
                let (label, replacement) = match entry.kind {
                    CompletionKind::Dir => {
                        (format!("@{}/", entry.value), format!("@{}/", entry.value))
                    }
                    _ => (format!("@{}", entry.value), format!("@{} ", entry.value)),
                };
                CompletionItem {
                    kind: entry.kind,
                    label,
                    detail: entry.detail.clone(),
                    range: mention.range.clone(),
                    replacement,
                }
            })
            .collect()
    }
}

/// Lower sorts first: match tier, path depth, then length. An empty query
/// browses the project root alphabetically.
fn score(key: &str, query: &str) -> Option<(u8, usize, usize)> {
    let depth = key.matches('/').count();
    if query.is_empty() {
        return (depth == 0).then_some((0, 0, 0));
    }
    let name = key.rsplit('/').next().unwrap_or(key);
    let tier = if key.starts_with(query) || name.starts_with(query) {
        0
    } else if name.contains(query) {
        1
    } else if key.contains(query) {
        2
    } else if is_subsequence(key, query) {
        3
    } else {
        return None;
    };
    Some((tier, depth, key.len()))
}

fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.any(|c| c == wanted))
}

/// Finds an `@token` that starts at a word boundary and contains `cursor` (a byte offset).
pub fn mention_at(input: &str, cursor: usize) -> Option<MentionQuery<'_>> {
    let mut cursor = cursor.min(input.len());
    while !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let start = input[..cursor]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(index, c)| index + c.len_utf8());
    let query = input[start..cursor].strip_prefix('@')?;
    let end = input[cursor..]
        .find(char::is_whitespace)
        .map_or(input.len(), |offset| cursor + offset);
    Some(MentionQuery {
        query,
        range: start..end,
    })
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

/// Appends the body of every mentioned skill so the agent follows it this turn.
///
/// A project path with the same name wins, keeping `@docs` a folder reference.
///
/// # Errors
/// Returns the name of a mentioned skill whose body cannot be read.
pub fn expand_skill_mentions(
    text: &str,
    root: Option<&Path>,
    skills: &SkillRegistry,
) -> Result<String, String> {
    let mut expanded = text.to_owned();
    for name in mentioned_names(text) {
        if skills.get(name).is_none() || root.is_some_and(|root| root.join(name).exists()) {
            continue;
        }
        let body = skills.load_body(name).map_err(|_| name.to_owned())?;
        expanded.push_str(&format!(
            "{SKILL_BLOCK_START}{name}\">\n{}{SKILL_BLOCK_END}",
            body.trim_end()
        ));
    }
    Ok(expanded)
}

/// Splits trailing skill blocks added by [`expand_skill_mentions`] from the typed text.
pub fn split_skill_attachments(text: &str) -> (&str, Vec<&str>) {
    for (start, _) in text.match_indices(SKILL_BLOCK_START) {
        if let Some(names) = parse_skill_blocks(&text[start..]) {
            return (&text[..start], names);
        }
    }
    (text, Vec::new())
}

fn parse_skill_blocks(rest: &str) -> Option<Vec<&str>> {
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let (name, body) = rest.strip_prefix(SKILL_BLOCK_START)?.split_once("\">\n")?;
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    for (end, _) in body.match_indices(SKILL_BLOCK_END) {
        if let Some(mut names) = parse_skill_blocks(&body[end + SKILL_BLOCK_END.len()..]) {
            names.insert(0, name);
            return Some(names);
        }
    }
    None
}

fn list_files(root: &Path) -> Vec<String> {
    let mut files = git_files(root).unwrap_or_else(|| {
        let mut files = Vec::new();
        walk(root, root, &mut files);
        files
    });
    files.sort();
    files.dedup();
    files
}

/// Tracked plus untracked-but-not-ignored files, honouring `.gitignore`.
fn git_files(root: &Path) -> Option<Vec<String>> {
    let mut command = std::process::Command::new("git");
    for variable in GIT_LOCAL_ENV {
        command.env_remove(variable);
    }
    let output = command
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|path| std::str::from_utf8(path).ok())
            .filter(|path| !path.is_empty() && root.join(path).exists())
            .take(MAX_INDEXED_PATHS)
            .map(str::to_owned)
            .collect(),
    )
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= MAX_INDEXED_PATHS {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()) {
                walk(root, &path, files);
            }
        } else if let Some(relative) = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .map(|path| path.replace(std::path::MAIN_SEPARATOR, "/"))
        {
            files.push(relative);
        }
    }
}

#[cfg(test)]
#[path = "composer_mention_tests.rs"]
mod tests;
