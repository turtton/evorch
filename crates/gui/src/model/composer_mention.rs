//! `@` mentions: project file/skill index, cursor token detection and skill expansion.
//!
//! Files under the project's allowed directories are indexed too and inserted
//! as absolute paths, the form the agent is told those directories by. Each
//! path is indexed once: the project root keeps its relative form, and an
//! allowed directory nested in another lists its own files, so what the outer
//! one ignores or skips still completes once approved.

use std::collections::BTreeSet;
use std::ops::Range;
use std::path::{Path, PathBuf};

use runtime::skill::SkillRegistry;

/// Upper bound on indexed paths, across the project and its allowed
/// directories, so huge checkouts keep matching per keystroke cheap.
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
    /// Repo-relative path with `/` separators, an absolute path inside an
    /// allowed directory, or the skill name.
    pub value: String,
    pub detail: String,
    /// Lowercased `value`; a query that prefixes it browses by full path.
    key: String,
    /// Lowercased name for fuzzy matching and depth: `key` itself, or for an
    /// allowed directory its folder name plus the path inside it, so `@shared`
    /// finds it without the absolute prefix matching unrelated queries.
    search: String,
    /// Lies in an allowed directory, so `value` is an absolute path.
    in_allowed: bool,
}

impl MentionEntry {
    fn new(kind: CompletionKind, value: String, detail: String) -> Self {
        let key = value.to_lowercase();
        Self {
            kind,
            search: key.clone(),
            key,
            value,
            detail,
            in_allowed: false,
        }
    }

    fn allowed(kind: CompletionKind, value: String, search: &str) -> Self {
        let detail = match kind {
            CompletionKind::Dir => "allowed folder",
            _ => "allowed file",
        };
        Self {
            kind,
            key: value.to_lowercase(),
            search: search.to_lowercase(),
            value,
            detail: detail.into(),
            in_allowed: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MentionIndex {
    pub root: Option<PathBuf>,
    /// Allowed directories the index was built for, as given to
    /// [`Self::build`]. Dropping some does not change this list, so the index
    /// still reads as built for an older scope until it is rebuilt.
    pub allowed: Vec<PathBuf>,
    /// Those of `allowed` whose entries are still offered: what
    /// [`Self::retain_allowed`] has not dropped. Allowed-directory entries lie
    /// only within these.
    retained: Vec<PathBuf>,
    /// Whether the skills it was built from include the project's repository
    /// skills, as its trust then admitted. Like `allowed`, this keeps what it
    /// was built for after [`Self::withhold_repo_skills`] drops skills.
    pub repo_skills: bool,
    /// Whether [`Self::withhold_repo_skills`] dropped the skill entries.
    skills_withheld: bool,
    pub entries: Vec<MentionEntry>,
}

/// The `@token` under the cursor; `range` covers the whole token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionQuery<'a> {
    pub query: &'a str,
    pub range: Range<usize>,
}

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
        let files = list_files(root, &[], limit, false);
        let mut budget = limit.saturating_sub(files.len());
        let mut index = Self::from_parts(Some(root.to_owned()), &files, skills);
        let listed = distinct_allowed(root, allowed);
        for &directory in &listed {
            // The project's files keep their relative entries, and a nested
            // allowed directory lists its own.
            let excluded: Vec<&Path> = std::iter::once(root)
                .chain(listed.iter().copied())
                .filter(|inner| *inner != directory && inner.starts_with(directory))
                .collect();
            let files = list_files(directory, &excluded, budget, true);
            budget = budget.saturating_sub(files.len());
            index.add_allowed_directory(directory, &files);
        }
        index.allowed = allowed.to_vec();
        index.retained = allowed.to_vec();
        index.repo_skills = repo_skills;
        index
    }

    /// Adds `directory` itself, its folders and `files` (relative to it) as
    /// absolute-path entries.
    fn add_allowed_directory(&mut self, directory: &Path, files: &[String]) {
        let Some(base) = slash_base(directory) else {
            return;
        };
        let name = base
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(&base)
            .to_owned();
        let dirs: BTreeSet<&str> = files
            .iter()
            .flat_map(|file| {
                file.match_indices('/')
                    .map(move |(index, _)| &file[..index])
            })
            .collect();
        let relative = std::iter::once((CompletionKind::Dir, ""))
            .chain(dirs.into_iter().map(|dir| (CompletionKind::Dir, dir)))
            .chain(
                files
                    .iter()
                    .map(|file| (CompletionKind::File, file.as_str())),
            );
        for (kind, path) in relative {
            let (value, search) = if path.is_empty() {
                (base.clone(), name.clone())
            } else {
                (format!("{base}/{path}"), format!("{name}/{path}"))
            };
            self.entries
                .push(MentionEntry::allowed(kind, value, &search));
        }
    }

    /// Drops the entries of allowed directories no longer in `allowed`, so a
    /// revoked directory stops completing at once. The index keeps the list
    /// it was built for, so [`Self::is_built_for`] calls for a rebuild that
    /// hands the freed path budget to the directories still allowed.
    pub fn retain_allowed(&mut self, allowed: &[PathBuf]) {
        if self
            .retained
            .iter()
            .all(|directory| allowed.contains(directory))
        {
            return;
        }
        self.retained
            .retain(|directory| allowed.contains(directory));
        let bases: Vec<_> = self
            .retained
            .iter()
            .filter_map(|directory| slash_base(directory))
            .collect();
        self.entries.retain(|entry| {
            !entry.in_allowed || bases.iter().any(|base| is_within(&entry.value, base))
        });
    }

    /// Drops every skill once the project no longer admits its repository
    /// skills, so none of them completes before the rebuild lands. Which
    /// skills came from the repository is not recorded, and a user skill one
    /// of them shadowed only returns with the rebuild anyway, so the user and
    /// builtin skills wait for it too. [`Self::is_built_for`] then calls for
    /// that rebuild whichever trust it is asked about.
    pub fn withhold_repo_skills(&mut self) {
        if !self.repo_skills || self.skills_withheld {
            return;
        }
        self.entries
            .retain(|entry| entry.kind != CompletionKind::Skill);
        self.skills_withheld = true;
    }

    /// Whether this is what [`Self::build`] gives for `root`, `allowed` and
    /// `repo_skills`, short of files changed since: built for exactly them,
    /// with no directory's entries or skills dropped afterwards.
    pub fn is_built_for(&self, root: &Path, allowed: &[PathBuf], repo_skills: bool) -> bool {
        self.root.as_deref() == Some(root)
            && self.allowed == allowed
            && self.retained == allowed
            && self.repo_skills == repo_skills
            && !self.skills_withheld
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
        Self {
            root,
            allowed: Vec::new(),
            retained: Vec::new(),
            repo_skills: false,
            skills_withheld: false,
            entries,
        }
    }

    pub fn complete(&self, mention: &MentionQuery<'_>) -> Vec<CompletionItem> {
        let query = mention.query.to_lowercase();
        let mut scored: Vec<_> = self
            .entries
            .iter()
            .filter_map(|entry| score(entry, &query).map(|score| (score, entry)))
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
/// browses the project root and the allowed directories alphabetically.
fn score(entry: &MentionEntry, query: &str) -> Option<(u8, usize, usize)> {
    let search = entry.search.as_str();
    let depth = search.matches('/').count();
    if query.is_empty() {
        return (depth == 0).then_some((0, 0, 0));
    }
    let name = search.rsplit('/').next().unwrap_or(search);
    let tier =
        if entry.key.starts_with(query) || search.starts_with(query) || name.starts_with(query) {
            0
        } else if name.contains(query) {
            1
        } else if search.contains(query) {
            2
        } else if is_subsequence(search, query) {
            3
        } else {
            return None;
        };
    Some((tier, depth, search.len()))
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
        if !inlines(name, root, skills) {
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

/// The mentioned names, once each in first-seen order, that
/// [`expand_skill_mentions`] leaves as text with `skills` yet would inline
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

/// `/`-separated form of a native path string.
fn slash_path(path: &str) -> String {
    path.replace(std::path::MAIN_SEPARATOR, "/")
}

/// The `/`-separated prefix of the absolute entries under `directory`.
fn slash_base(directory: &Path) -> Option<String> {
    let base = slash_path(directory.to_str()?);
    Some(base.trim_end_matches('/').to_owned())
}

/// Whether `/`-separated `path` is `base` or lies under it.
fn is_within(path: &str, base: &str) -> bool {
    path.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The `allowed` directories that list paths of their own, each once: those
/// that exist and lie outside `root`. One nested in another still lists its
/// own, since the outer listing may ignore or skip it.
/// Checking existence here keeps filesystem access off the UI thread.
fn distinct_allowed<'a>(root: &Path, allowed: &'a [PathBuf]) -> Vec<&'a Path> {
    let mut distinct: Vec<&Path> = Vec::new();
    for directory in allowed {
        if !directory.starts_with(root)
            && !distinct.contains(&directory.as_path())
            && directory.is_dir()
        {
            distinct.push(directory);
        }
    }
    distinct
}

/// Up to `limit` files under `root`, relative to it with `/` separators.
/// The `excluded` subtrees are skipped before they can use up the limit.
/// An `approved` root is an allowed directory: when the repository around it
/// ignores it, git lists nothing, yet approving it asks for its files, so it
/// is walked instead. The project root keeps git's answer.
fn list_files(root: &Path, excluded: &[&Path], limit: usize, approved: bool) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }
    let listed = if approved && git_ignores(root) {
        None
    } else {
        git_files(root, excluded, limit)
    };
    let mut files = listed.unwrap_or_else(|| {
        let mut files = Vec::new();
        walk(root, root, excluded, limit, &mut files);
        files
    });
    files.sort();
    files.dedup();
    files
}

/// `git` run in `dir`, with no inherited hook environment redirecting it.
fn git(dir: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    for variable in GIT_LOCAL_ENV {
        command.env_remove(variable);
    }
    command
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// Whether the repository `dir` lies in ignores `dir` itself. Outside any
/// repository, or for a repository's own top folder, it does not.
fn git_ignores(dir: &Path) -> bool {
    git(dir)
        .args(["check-ignore", "-q", "."])
        .stdout(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Tracked plus untracked-but-not-ignored files, honouring `.gitignore`.
fn git_files(root: &Path, excluded: &[&Path], limit: usize) -> Option<Vec<String>> {
    // git prints `/`-separated paths relative to `root`; compare in that form.
    let excluded: Vec<String> = excluded
        .iter()
        .filter_map(|excluded| excluded.strip_prefix(root).ok())
        .filter_map(Path::to_str)
        .map(slash_path)
        .filter(|excluded| !excluded.is_empty())
        .collect();
    let output = git(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
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
            .filter(|path| {
                !path.is_empty()
                    && !excluded.iter().any(|excluded| is_within(path, excluded))
                    && root.join(path).exists()
            })
            .take(limit)
            .map(str::to_owned)
            .collect(),
    )
}

fn walk(root: &Path, dir: &Path, excluded: &[&Path], limit: usize, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if files.len() >= limit {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !SKIPPED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
                && !excluded.contains(&path.as_path())
            {
                walk(root, &path, excluded, limit, files);
            }
        } else if let Some(relative) = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .map(slash_path)
        {
            files.push(relative);
        }
    }
}

#[cfg(test)]
#[path = "composer_mention_tests.rs"]
mod tests;
