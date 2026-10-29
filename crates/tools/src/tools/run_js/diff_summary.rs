//! One-line summaries of the diffs `edit`, `write` and `git_diff` return (ADR 0039).
//!
//! Inside a normal call the model reads the diff. Inside `run_js` it usually
//! prints the reply of an edit "to see what happened", which fills its context
//! with a diff it just wrote itself. The program therefore gets a summary in
//! `text` (`edited <path> (+N −M)`) and the complete diff in `detail.diff`.

use serde_json::{Value, json};

/// Notes `edit` and the post-edit checker append after the diff. They are
/// addressed to the model and stay in `text`, after the summary. A diff line
/// always starts with ` `, `+`, `-`, `@` or `\`, so these cannot occur inside one.
const TRAILERS: [&str; 2] = [
    "\nNote: ignored old_string/new_string/replace_all because edits was given",
    "\n[comment-checker warning]",
];

/// Files listed in a `git_diff` summary; the rest are counted.
const LISTED_FILES: usize = 100;

/// A summarised reply.
pub(super) struct Summary {
    /// What `text` becomes.
    pub text: String,
    /// Added to `detail`: the complete diff and its counts.
    pub detail: Value,
}

/// Which file-writing tool a diff comes from.
#[derive(Clone, Copy)]
pub(super) enum Writer {
    Edit,
    Write,
}

/// Summarises the unified diff of one file that `edit` or `write` returned, or
/// `None` when the reply is not such a diff (`No changes to …`, `diff omitted`, …)
/// and is short already.
pub(super) fn summarize_file(writer: Writer, path: &str, text: &str) -> Option<Summary> {
    let split = TRAILERS
        .iter()
        .filter_map(|trailer| text.find(trailer))
        .min()
        .unwrap_or(text.len());
    let (diff, trailer) = text.split_at(split);
    let created = diff.starts_with("--- /dev/null\n");
    if !created && !diff.starts_with("--- ") {
        return None;
    }
    let (added, removed) = count_lines(diff.lines().skip(2));
    let counts = if created {
        format!("(+{added})")
    } else {
        format!("(+{added} −{removed})")
    };
    let summary = match writer {
        Writer::Edit => format!("edited {path} {counts}"),
        Writer::Write if created => format!("created {path} {counts}"),
        Writer::Write => format!("wrote {path} {counts}"),
    };
    Some(Summary {
        text: format!("{summary}{trailer}"),
        detail: json!({"diff": diff, "added": added, "removed": removed}),
    })
}

/// Summarises the output of `git diff` per file, or `None` when it is empty or
/// not in git's format.
pub(super) fn summarize_git_diff(text: &str) -> Option<Summary> {
    let files = split_files(text);
    if files.is_empty() {
        return None;
    }
    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
    let mut lines = vec![format!(
        "{} file{} changed (+{added} −{removed})",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    )];
    for file in files.iter().take(LISTED_FILES) {
        let name = match &file.old_path {
            Some(old) => format!("{} (renamed from {old})", file.path),
            None => file.path.clone(),
        };
        lines.push(if file.binary {
            format!("{name} (binary)")
        } else {
            format!("{name} (+{} −{})", file.added, file.removed)
        });
    }
    if files.len() > LISTED_FILES {
        lines.push(format!("… {} more files", files.len() - LISTED_FILES));
    }
    let listed: Vec<Value> = files
        .iter()
        .map(|file| {
            json!({"path": file.path, "old_path": file.old_path, "added": file.added, "removed": file.removed, "binary": file.binary})
        })
        .collect();
    Some(Summary {
        text: lines.join("\n"),
        detail: json!({"diff": text, "files": listed}),
    })
}

struct FileStats {
    path: String,
    /// The path before a rename or copy.
    old_path: Option<String>,
    added: u64,
    removed: u64,
    binary: bool,
}

/// What a file's header says about its name; the first source present wins
/// (`rename to`, then `+++`, then `---`, then the `diff --git` line itself).
#[derive(Default)]
struct Names {
    from_line: Option<(String, String)>,
    rename_from: Option<String>,
    rename_to: Option<String>,
    old: Option<String>,
    new: Option<String>,
    prefixed: bool,
}

impl Names {
    fn finish(self) -> (String, Option<String>) {
        let prefixed = self.prefixed;
        let strip = move |name: String, prefix: &str| match name.strip_prefix(prefix) {
            Some(rest) if prefixed => rest.to_owned(),
            _ => name,
        };
        let (line_old, line_new) = self.from_line.unwrap_or_default();
        let path = self
            .rename_to
            .or_else(|| self.new.map(|name| strip(name, "b/")))
            .or_else(|| self.old.clone().map(|name| strip(name, "a/")))
            .unwrap_or_else(|| strip(line_new, "b/"));
        let old_path = self
            .rename_from
            .or_else(|| (!line_old.is_empty()).then(|| strip(line_old, "a/")))
            .filter(|old| *old != path);
        (path, old_path)
    }
}

/// Splits `git diff` output at its `diff --git` lines. Such a line at the start of
/// a line is always a header: the lines of a hunk begin with ` `, `+`, `-` or `\`.
fn split_files(text: &str) -> Vec<FileStats> {
    let mut files: Vec<FileStats> = Vec::new();
    let mut names = Names::default();
    let mut in_hunk = false;
    let flush = |files: &mut Vec<FileStats>, names: &mut Names| {
        if let Some(file) = files.last_mut() {
            let (path, old_path) = std::mem::take(names).finish();
            file.path = path;
            file.old_path = old_path;
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut files, &mut names);
            names = Names {
                from_line: split_names(rest),
                prefixed: rest.starts_with("a/") || rest.starts_with("\"a/"),
                ..Names::default()
            };
            files.push(FileStats {
                path: String::new(),
                old_path: None,
                added: 0,
                removed: 0,
                binary: false,
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            match line.as_bytes().first() {
                Some(b'+') => file.added += 1,
                Some(b'-') => file.removed += 1,
                _ => {}
            }
        } else if let Some(name) = line.strip_prefix("rename from ") {
            names.rename_from = Some(unquote(name));
        } else if let Some(name) = line.strip_prefix("rename to ") {
            names.rename_to = Some(unquote(name));
        } else if let Some(name) = line.strip_prefix("--- ") {
            names.old = header_name(name);
        } else if let Some(name) = line.strip_prefix("+++ ") {
            names.new = header_name(name);
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
        }
    }
    flush(&mut files, &mut names);
    files
}

/// The name in a `---` / `+++` line: `None` for `/dev/null`; git ends a name that
/// contains a space with a tab.
fn header_name(raw: &str) -> Option<String> {
    let name = if raw.starts_with('"') {
        unquote(raw)
    } else {
        raw.trim_end_matches('\t').to_owned()
    };
    (name != "/dev/null").then_some(name)
}

/// The two names of `a/<old> b/<new>`, quoted or not. Unquoted names with spaces are
/// ambiguous; the split where both sides name the same file is preferred (no
/// rename), else the last one.
fn split_names(names: &str) -> Option<(String, String)> {
    if names.starts_with('"') {
        let (first, rest) = take_quoted(names)?;
        let rest = rest.trim_start();
        let second = if rest.starts_with('"') {
            take_quoted(rest)?.0
        } else {
            rest.to_owned()
        };
        return Some((first, second));
    }
    let candidates: Vec<usize> = names.match_indices(" b/").map(|(at, _)| at).collect();
    let pick = candidates
        .iter()
        .find(|&&at| names[..at].strip_prefix("a/") == Some(&names[at + 3..]))
        .or_else(|| candidates.last())?;
    let (old, new) = names.split_at(*pick);
    let new = &new[1..];
    Some((old.to_owned(), new.to_owned()))
}

/// Takes a leading C-style quoted name and returns it decoded with the remainder.
fn take_quoted(text: &str) -> Option<(String, &str)> {
    let body = text.strip_prefix('"')?;
    let mut escaped = false;
    for (at, ch) in body.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                return Some((decode_c_quoted(&body[..at]), &body[at + 1..]));
            }
            _ => escaped = false,
        }
    }
    None
}

/// A name as git prints it: plain, or quoted with C escapes when it holds
/// control characters, quotes, backslashes or (by default) non-ASCII bytes.
fn unquote(name: &str) -> String {
    if name.starts_with('"') {
        take_quoted(name).map_or_else(|| name.to_owned(), |(decoded, _)| decoded)
    } else {
        name.to_owned()
    }
}

fn decode_c_quoted(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' || at + 1 >= bytes.len() {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        at += 1;
        match bytes[at] {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            digit @ b'0'..=b'7' => {
                let mut value = u32::from(digit - b'0');
                let mut taken = 1;
                while taken < 3 && at + 1 < bytes.len() && matches!(bytes[at + 1], b'0'..=b'7') {
                    at += 1;
                    value = value * 8 + u32::from(bytes[at] - b'0');
                    taken += 1;
                }
                out.push(u8::try_from(value & 0xff).unwrap_or(0));
            }
            other => out.push(other),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn count_lines<'a>(lines: impl Iterator<Item = &'a str>) -> (u64, u64) {
    lines.fold((0, 0), |(added, removed), line| {
        match line.as_bytes().first() {
            Some(b'+') => (added + 1, removed),
            Some(b'-') => (added, removed + 1),
            _ => (added, removed),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT: &str = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n keep\n-old one\n-old two\n+new one\n+new two\n+new three\n";

    #[test]
    fn an_edit_becomes_one_line_and_keeps_the_diff_in_detail() {
        let summary = summarize_file(Writer::Edit, "src/lib.rs", EDIT).unwrap();
        assert_eq!(summary.text, "edited src/lib.rs (+3 −2)");
        assert_eq!(
            summary.detail,
            json!({"diff": EDIT, "added": 3, "removed": 2})
        );
    }

    #[test]
    fn a_hunk_line_that_looks_like_a_header_is_still_counted() {
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n--- not a header\n+++ not a header either\n";
        let summary = summarize_file(Writer::Edit, "f", diff).unwrap();
        assert_eq!(summary.text, "edited f (+1 −1)");
    }

    #[test]
    fn write_tells_a_new_file_from_a_replaced_one() {
        let created = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+a\n+b\n";
        assert_eq!(
            summarize_file(Writer::Write, "new.txt", created)
                .unwrap()
                .text,
            "created new.txt (+2)"
        );
        assert_eq!(
            summarize_file(Writer::Write, "src/lib.rs", EDIT)
                .unwrap()
                .text,
            "wrote src/lib.rs (+3 −2)"
        );
    }

    #[test]
    fn notes_after_the_diff_stay_after_the_summary() {
        let ignored = format!(
            "{EDIT}\nNote: ignored old_string/new_string/replace_all because edits was given; only edits were applied."
        );
        let summary = summarize_file(Writer::Edit, "src/lib.rs", &ignored).unwrap();
        assert!(
            summary
                .text
                .starts_with("edited src/lib.rs (+3 −2)\nNote: ignored")
        );
        assert_eq!(summary.detail["diff"], EDIT);

        let warned =
            format!("{EDIT}\n[comment-checker warning]\n> x\n[end comment-checker warning]");
        let summary = summarize_file(Writer::Edit, "src/lib.rs", &warned).unwrap();
        assert!(summary.text.ends_with("[end comment-checker warning]"));
        assert_eq!(summary.detail["added"], 3);
    }

    #[test]
    fn replies_that_are_not_a_diff_are_left_alone() {
        for text in [
            "No changes to a.txt",
            "Created empty file a.txt",
            "Wrote a.txt; diff unavailable: previous file exceeds 2097152 bytes",
            "Changed a.txt; diff omitted because the file exceeds 2097152 bytes",
            "",
        ] {
            assert!(
                summarize_file(Writer::Write, "a.txt", text).is_none(),
                "{text}"
            );
        }
    }

    const GIT: &str = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,3 @@\n x\n-y\n+z\n+w\ndiff --git a/dir/b c.txt b/dir/b c.txt\nnew file mode 100644\n--- /dev/null\n+++ b/dir/b c.txt\n@@ -0,0 +1 @@\n+hello\ndiff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n";

    #[test]
    fn git_diff_is_summarised_per_file() {
        let summary = summarize_git_diff(GIT).unwrap();
        assert_eq!(
            summary.text,
            "3 files changed (+3 −1)\na.rs (+2 −1)\ndir/b c.txt (+1 −0)\nimg.png (binary)"
        );
        assert_eq!(summary.detail["diff"], GIT);
        assert_eq!(
            summary.detail["files"][1],
            json!({"path": "dir/b c.txt", "old_path": null, "added": 1, "removed": 0, "binary": false})
        );
        assert_eq!(summary.detail["files"][2]["binary"], true);
    }

    fn names(diff: &str) -> Vec<(String, Option<String>)> {
        split_files(diff)
            .into_iter()
            .map(|file| (file.path, file.old_path))
            .collect()
    }

    #[test]
    fn quoted_non_ascii_and_escaped_names_are_decoded() {
        let diff = concat!(
            "diff --git \"a/caf\\303\\251 d\\303\\274.txt\" \"b/caf\\303\\251 d\\303\\274.txt\"\n",
            "--- \"a/caf\\303\\251 d\\303\\274.txt\"\n+++ \"b/caf\\303\\251 d\\303\\274.txt\"\n@@ -1 +1 @@\n-a\n+b\n",
            "diff --git \"a/q\\\"uo\\\\te\\tab.txt\" \"b/q\\\"uo\\\\te\\tab.txt\"\nBinary files differ\n",
        );
        assert_eq!(
            names(diff),
            [
                ("café dü.txt".to_owned(), None),
                ("q\"uo\\te\tab.txt".to_owned(), None)
            ]
        );
        let summary = summarize_git_diff(diff).unwrap();
        assert!(
            summary.text.contains("\ncafé dü.txt (+1 −1)\n"),
            "{}",
            summary.text
        );
    }

    #[test]
    fn renames_and_deletions_use_the_header_names() {
        let diff = concat!(
            "diff --git a/old name.rs b/new name.rs\nsimilarity index 90%\nrename from old name.rs\nrename to new name.rs\n",
            "--- a/old name.rs\t\n+++ b/new name.rs\t\n@@ -1 +1 @@\n-a\n+b\n",
            "diff --git \"a/\\303\\251.rs\" \"b/\\303\\251.rs\"\ndeleted file mode 100644\n--- \"a/\\303\\251.rs\"\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
            "diff --git a/moved.rs b/dir/moved.rs\nsimilarity index 100%\nrename from moved.rs\nrename to dir/moved.rs\n",
        );
        assert_eq!(
            names(diff),
            [
                ("new name.rs".to_owned(), Some("old name.rs".to_owned())),
                ("é.rs".to_owned(), None),
                ("dir/moved.rs".to_owned(), Some("moved.rs".to_owned())),
            ]
        );
        let summary = summarize_git_diff(diff).unwrap();
        assert!(
            summary
                .text
                .contains("new name.rs (renamed from old name.rs) (+1 −1)")
        );
        assert!(
            summary
                .text
                .contains("dir/moved.rs (renamed from moved.rs) (+0 −0)")
        );
    }

    #[test]
    fn a_header_line_without_a_prefix_keeps_the_names_whole() {
        let diff = "diff --git b/x.rs b/x.rs\n--- b/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(names(diff), [("b/x.rs".to_owned(), None)]);
        let prefixed =
            "diff --git a/b/x.rs b/b/x.rs\n--- a/b/x.rs\n+++ b/b/x.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(names(prefixed), [("b/x.rs".to_owned(), None)]);
    }

    #[test]
    fn a_name_with_spaces_and_no_header_lines_is_split_where_both_sides_agree() {
        let diff = "diff --git a/x b/y b/x b/y\nBinary files a/x b/y and b/x b/y differ\n";
        assert_eq!(names(diff), [("x b/y".to_owned(), None)]);
    }

    #[test]
    fn names_in_the_output_of_a_real_git_are_decoded() {
        let directory = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        for name in ["café.txt", "with space.txt", "quo\"te.txt", "gone.txt"] {
            std::fs::write(directory.path().join(name), "one\n").unwrap();
        }
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "base",
        ]);
        for name in ["café.txt", "with space.txt", "quo\"te.txt"] {
            std::fs::write(directory.path().join(name), "two\nthree\n").unwrap();
        }
        std::fs::remove_file(directory.path().join("gone.txt")).unwrap();

        let summary = summarize_git_diff(&git(&["diff"])).unwrap();

        let paths: Vec<&str> = summary.detail["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            ["café.txt", "gone.txt", "quo\"te.txt", "with space.txt"]
        );
        assert!(
            summary.text.starts_with("4 files changed (+6 −4)\n"),
            "{}",
            summary.text
        );
    }

    #[test]
    fn an_empty_or_unrecognised_git_diff_is_left_alone() {
        assert!(summarize_git_diff("").is_none());
        assert!(summarize_git_diff("something else\n").is_none());
    }

    #[test]
    fn a_long_file_list_is_cut_and_counted() {
        let many: String = (0..LISTED_FILES + 5)
            .map(|index| {
                format!("diff --git a/f{index} b/f{index}\n--- a/f{index}\n+++ b/f{index}\n@@ -1 +1 @@\n-a\n+b\n")
            })
            .collect();
        let summary = summarize_git_diff(&many).unwrap();
        assert!(summary.text.starts_with("105 files changed (+105 −105)\n"));
        assert!(summary.text.ends_with("… 5 more files"));
        assert_eq!(summary.detail["files"].as_array().unwrap().len(), 105);
    }
}
