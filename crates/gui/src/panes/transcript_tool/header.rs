//! One-line tool call header: `<icon> <Verb> <subject> <location> · <meta>`.
//!
//! The subject (file name, command, pattern) carries the meaning and is the
//! only emphasized part; location and meta are muted context that may be
//! shortened when space runs out.

use std::path::{Component, Path, PathBuf};

use crate::model::transcript::shell_jobs::ShellJob;
use crate::theme::icons;

const MAX_SUBJECT_CHARS: usize = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolHeader {
    pub icon: &'static str,
    pub verb: String,
    pub subject: String,
    /// Directory or search root; shortened from the left when it does not fit.
    pub location: Option<String>,
    /// Outcome or range hints such as `L10–59`, `4 matches`, `+3 −1`.
    pub meta: Option<String>,
}

impl ToolHeader {
    /// Plain text used for the accessible name and hover text.
    pub fn text(&self) -> String {
        let mut text = self.verb.clone();
        for part in [Some(&self.subject), self.location.as_ref()]
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
        {
            text.push(' ');
            text.push_str(part);
        }
        if let Some(meta) = &self.meta {
            text.push_str(" · ");
            text.push_str(meta);
        }
        text
    }
}

pub fn tool_header(
    tool_name: &str,
    input: Option<&serde_json::Value>,
    output: Option<&str>,
    is_error: bool,
    repo_root: Option<&Path>,
) -> ToolHeader {
    // Error text is not a diff or match list; only shell exit codes stay.
    let result = output.filter(|_| !is_error);
    let str_arg = |key: &str| input.and_then(|input| input.get(key)?.as_str());
    let file_arg = || {
        [
            "path",
            "file_path",
            "filePath",
            "file",
            "filename",
            "target",
        ]
        .iter()
        .find_map(|key| str_arg(key))
    };
    let header = |icon, verb: &str| ToolHeader {
        icon,
        verb: verb.to_owned(),
        subject: String::new(),
        location: None,
        meta: None,
    };
    match tool_name {
        "read" => {
            let mut header = header(icons::FILE_TEXT, "Read");
            if let Some(path) = file_arg() {
                (header.subject, header.location) = split_path(path, repo_root);
            }
            header.meta = input.and_then(read_range);
            header
        }
        "edit" | "write" => {
            let verb = if tool_name == "edit" { "Edit" } else { "Write" };
            let icon = if tool_name == "edit" {
                icons::PENCIL_SIMPLE
            } else {
                icons::FILE_PLUS
            };
            let mut header = header(icon, verb);
            if let Some(path) = file_arg() {
                (header.subject, header.location) = split_path(path, repo_root);
            }
            header.meta = result.and_then(diff_stat);
            header
        }
        "grep" => {
            let mut header = header(icons::MAGNIFYING_GLASS, "Grep");
            if let Some(pattern) = str_arg("pattern") {
                header.subject = one_line(&format!("\"{pattern}\""));
            }
            header.location =
                str_arg("path").map(|path| format!("in {}", display_path(path, repo_root)));
            header.meta = result.and_then(grep_matches);
            header
        }
        "bash" | "shell" => {
            let mut header = header(icons::TERMINAL_WINDOW, "Shell");
            match (str_arg("action"), str_arg("job_id")) {
                (Some(action @ ("poll" | "stdin" | "stop")), Some(job)) => {
                    header.verb = control_verb(action).into();
                    header.subject = format!("#{}", short_job_id(job));
                }
                _ => header.subject = str_arg("command").map(one_line).unwrap_or_default(),
            }
            header.meta = output.and_then(failed_exit_code);
            header
        }
        "web_fetch" => {
            let mut header = header(icons::GLOBE, "Fetch");
            header.subject = str_arg("url").map(one_line).unwrap_or_default();
            header
        }
        "web_search" => {
            let mut header = header(icons::GLOBE_SIMPLE, "Search");
            if let Some(query) = str_arg("query") {
                header.subject = one_line(&format!("\"{query}\""));
            }
            header
        }
        "git_diff" => {
            let mut header = header(icons::GIT_DIFF, "Git diff");
            header.subject = str_arg("path")
                .map(|path| display_path(path, repo_root))
                .unwrap_or_default();
            header
        }
        "skill_load" => {
            let mut header = header(icons::BOOK_OPEN, "Load skill");
            header.subject = str_arg("name").map(one_line).unwrap_or_default();
            header.location = str_arg("resource").map(one_line);
            header
        }
        "create_goal" => {
            let mut header = header(icons::TARGET, "Set goal");
            header.subject = str_arg("objective").map(one_line).unwrap_or_default();
            header.meta = input
                .and_then(|input| input.get("criteria")?.as_array())
                .map(|criteria| match criteria.len() {
                    1 => "1 criterion".to_owned(),
                    count => format!("{count} criteria"),
                });
            header
        }
        _ => header(icons::WRENCH, tool_name),
    }
}

/// Repo-relative display path. Paths inside a run worktree drop the
/// `.evorch/worktrees/<run>` prefix; paths elsewhere under `$HOME` use `~`.
pub fn display_path(path: &str, repo_root: Option<&Path>) -> String {
    let display = relative_path(Path::new(path), repo_root, home_dir().as_deref());
    let display = display.to_string_lossy();
    if display.is_empty() {
        ".".to_owned()
    } else {
        display.into_owned()
    }
}

fn relative_path(path: &Path, repo_root: Option<&Path>, home: Option<&Path>) -> PathBuf {
    if let Some(inner) = worktree_relative(path) {
        return inner;
    }
    if let Some(relative) = repo_root.and_then(|root| path.strip_prefix(root).ok()) {
        return relative.to_path_buf();
    }
    if let Some(relative) = home.and_then(|home| path.strip_prefix(home).ok()) {
        return Path::new("~").join(relative);
    }
    path.components()
        .skip_while(|component| matches!(component, Component::CurDir))
        .collect()
}

fn worktree_relative(path: &Path) -> Option<PathBuf> {
    let components: Vec<_> = path.components().collect();
    let start = components.windows(3).position(|window| {
        window[0].as_os_str() == ".evorch" && window[1].as_os_str() == "worktrees"
    })?;
    Some(components[start + 3..].iter().collect())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// `(file name, parent directory)` of the display path.
fn split_path(path: &str, repo_root: Option<&Path>) -> (String, Option<String>) {
    let display = display_path(path, repo_root);
    let display_path = Path::new(&display);
    match (display_path.file_name(), display_path.parent()) {
        (Some(name), parent) => (
            name.to_string_lossy().into_owned(),
            parent
                .map(|parent| parent.to_string_lossy().into_owned())
                .filter(|parent| !parent.is_empty()),
        ),
        (None, _) => (display, None),
    }
}

fn read_range(input: &serde_json::Value) -> Option<String> {
    let offset = input.get("offset").and_then(serde_json::Value::as_u64);
    let limit = input.get("limit").and_then(serde_json::Value::as_u64);
    match (offset, limit) {
        (Some(offset), Some(limit)) if limit > 0 => {
            Some(format!("L{offset}–{}", offset.saturating_add(limit - 1)))
        }
        (Some(offset), _) => Some(format!("from L{offset}")),
        (None, Some(limit)) if limit > 0 => Some(format!("L1–{limit}")),
        _ => None,
    }
}

/// `+added −removed` for a unified diff output; `None` for status messages.
fn diff_stat(output: &str) -> Option<String> {
    if !output.starts_with("--- ") {
        return None;
    }
    let (mut added, mut removed) = (0, 0);
    for line in output.lines() {
        if line.starts_with('+') && !line.starts_with("+++ ") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            removed += 1;
        }
    }
    Some(format!("+{added} −{removed}"))
}

fn grep_matches(output: &str) -> Option<String> {
    let mut shown = 0usize;
    let mut omitted = None;
    for line in output.lines().filter(|line| !line.is_empty()) {
        match line
            .strip_prefix("[truncated: ")
            .and_then(|rest| rest.strip_suffix(" more lines]"))
        {
            Some(rest) => omitted = Some(rest),
            None => shown += 1,
        }
    }
    let (total, at_least) = match omitted {
        Some(rest) => {
            let (at_least, count) = rest
                .strip_prefix("at least ")
                .map_or((false, rest), |count| (true, count));
            (shown + count.parse::<usize>().ok()?, at_least)
        }
        None => (shown, false),
    };
    let plus = if at_least { "+" } else { "" };
    let noun = if total == 1 { "match" } else { "matches" };
    Some(format!("{total}{plus} {noun}"))
}

const fn control_verb(action: &str) -> &'static str {
    match action.as_bytes() {
        b"poll" => "Poll",
        b"stdin" => "Input",
        _ => "Stop",
    }
}

fn short_job_id(job: &str) -> &str {
    job.get(..8).unwrap_or(job)
}

/// Names a shell job's command and state: control calls show the command
/// instead of the bare job ID, which moves to the location.
pub fn with_shell_job(header: &mut ToolHeader, job: &ShellJob, control: bool) {
    if control && let Some(command) = &job.command {
        header.location = Some(std::mem::replace(&mut header.subject, one_line(command)));
    }
    header.meta = Some(job.outcome());
}

fn failed_exit_code(output: &str) -> Option<String> {
    let code = output
        .lines()
        .next()?
        .strip_prefix("exit_code: ")?
        .parse::<i32>()
        .ok()?;
    (code != 0).then(|| format!("exit {code}"))
}

fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = joined.chars();
    let mut bounded: String = chars.by_ref().take(MAX_SUBJECT_CHARS).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// Drops leading directories (`a/b/c` → `…/b/c` → `…/c`) until `fits`
/// accepts the location or only its last directory remains.
pub fn shorten_location(location: &str, fits: impl Fn(&str) -> bool) -> String {
    if fits(location) {
        return location.to_owned();
    }
    let (prefix, rest) = location
        .strip_prefix("in ")
        .map_or(("", location), |rest| ("in ", rest));
    let parts: Vec<_> = rest.split('/').filter(|part| !part.is_empty()).collect();
    for skip in 1..parts.len() {
        let candidate = format!("{prefix}…/{}", parts[skip..].join("/"));
        if fits(&candidate) || skip + 1 == parts.len() {
            return candidate;
        }
    }
    location.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn header(tool: &str, input: serde_json::Value, output: Option<&str>) -> ToolHeader {
        tool_header(tool, Some(&input), output, false, Some(Path::new("/repo")))
    }

    #[test]
    fn read_keeps_file_name_as_subject_and_strips_run_worktree() {
        // Given: an absolute path inside a run worktree.
        let path = "/repo/.evorch/worktrees/run-7/crates/gui/src/lib.rs";
        // When
        let header = header(
            "read",
            json!({"path": path, "offset": 10, "limit": 50}),
            None,
        );
        // Then: the worktree prefix is gone and the file name leads.
        assert_eq!(header.text(), "Read lib.rs crates/gui/src · L10–59");
    }

    #[test]
    fn skill_loads_and_goals_name_what_the_agent_picked_up() {
        assert_eq!(
            header("skill_load", json!({"name": "review"}), None).text(),
            "Load skill review"
        );
        assert_eq!(
            header(
                "skill_load",
                json!({"name": "review", "resource": "references/checklist.md"}),
                None
            )
            .text(),
            "Load skill review references/checklist.md"
        );
        assert_eq!(
            header(
                "create_goal",
                json!({"objective": "Ship the\nsidebar", "criteria": ["a", "b"]}),
                None
            )
            .text(),
            "Set goal Ship the sidebar · 2 criteria"
        );
    }

    #[test]
    fn display_path_prefers_worktree_then_repo_then_home() {
        let home = Some(Path::new("/home/me"));
        let root = Some(Path::new("/home/me/repo"));
        for (path, expected) in [
            ("/elsewhere/.evorch/worktrees/run-1/src/a.rs", "src/a.rs"),
            ("/home/me/repo/src/a.rs", "src/a.rs"),
            ("/home/me/notes.md", "~/notes.md"),
            ("./src/a.rs", "src/a.rs"),
            ("/etc/hosts", "/etc/hosts"),
        ] {
            assert_eq!(
                relative_path(Path::new(path), root, home),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn read_range_shows_only_given_bounds() {
        for (input, expected) in [
            (json!({"path": "a"}), None),
            (json!({"path": "a", "offset": 5}), Some("from L5")),
            (json!({"path": "a", "limit": 20}), Some("L1–20")),
        ] {
            assert_eq!(header("read", input, None).meta.as_deref(), expected);
        }
    }

    #[test]
    fn edit_meta_counts_diff_lines_but_not_status_messages() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n keep\n";
        assert_eq!(
            header("edit", json!({"path": "x"}), Some(diff))
                .meta
                .as_deref(),
            Some("+2 −1")
        );
        assert_eq!(
            header("write", json!({"path": "x"}), Some("No changes to x")).meta,
            None
        );
    }

    #[test]
    fn grep_meta_counts_matches_including_truncated_lines() {
        for (output, expected) in [
            ("", "0 matches"),
            ("a.rs\u{0}1:x", "1 match"),
            (
                "a.rs\u{0}1:x\nb.rs\u{0}2:y\n[truncated: 3 more lines]",
                "5 matches",
            ),
            (
                "a.rs\u{0}1:x\n[truncated: at least 9 more lines]",
                "10+ matches",
            ),
        ] {
            let header = header(
                "grep",
                json!({"pattern": "x", "path": "/repo/src"}),
                Some(output),
            );
            assert_eq!(header.text(), format!("Grep \"x\" in src · {expected}"));
        }
    }

    #[test]
    fn shell_shows_command_and_only_failing_exit_codes() {
        let ok = "exit_code: 0\n--- stdout ---\nok\n--- stderr ---\n";
        let failed = "exit_code: 2\n--- stdout ---\n\n--- stderr ---\nboom";
        assert_eq!(
            header("shell", json!({"command": "cargo\n  test"}), Some(ok)).text(),
            "Shell cargo test"
        );
        assert_eq!(
            header("shell", json!({"command": "false"}), Some(failed)).text(),
            "Shell false · exit 2"
        );
        assert_eq!(
            header("shell", json!({"action": "poll", "job_id": "job-1"}), None).text(),
            "Poll #job-1"
        );
    }

    #[test]
    fn shell_job_controls_name_the_command_and_job_state() {
        let mut jobs = crate::model::transcript::shell_jobs::ShellJobs::default();
        let id = "3f9c2a1e-0000-4000-8000-000000000000";
        jobs.apply_result(
            Some(&json!({"command": "cargo\n test"})),
            None,
            Some(&json!({"shell_job": {"job_id": id, "status": "running"}})),
        );
        let job = jobs.get(id).unwrap();
        let mut poll = header("shell", json!({"action": "poll", "job_id": id}), None);
        with_shell_job(&mut poll, job, true);
        assert_eq!(poll.text(), "Poll cargo test #3f9c2a1e · running");
        let mut start = header("shell", json!({"command": "cargo test"}), None);
        with_shell_job(&mut start, job, false);
        assert_eq!(start.text(), "Shell cargo test · running");
    }

    #[test]
    fn shorten_location_drops_leading_directories_first() {
        let fits = |max: usize| move |text: &str| text.chars().count() <= max;
        assert_eq!(
            shorten_location("crates/gui/src", fits(20)),
            "crates/gui/src"
        );
        assert_eq!(shorten_location("crates/gui/src", fits(9)), "…/gui/src");
        assert_eq!(shorten_location("crates/gui/src", fits(1)), "…/src");
        assert_eq!(shorten_location("in crates/gui", fits(8)), "in …/gui");
    }
}
