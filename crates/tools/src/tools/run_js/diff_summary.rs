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
        lines.push(if file.binary {
            format!("{} (binary)", file.path)
        } else {
            format!("{} (+{} −{})", file.path, file.added, file.removed)
        });
    }
    if files.len() > LISTED_FILES {
        lines.push(format!("… {} more files", files.len() - LISTED_FILES));
    }
    let listed: Vec<Value> = files
        .iter()
        .map(|file| {
            json!({"path": file.path, "added": file.added, "removed": file.removed, "binary": file.binary})
        })
        .collect();
    Some(Summary {
        text: lines.join("\n"),
        detail: json!({"diff": text, "files": listed}),
    })
}

struct FileStats {
    path: String,
    added: u64,
    removed: u64,
    binary: bool,
}

/// Splits `git diff` output at its `diff --git` lines. Such a line at the start of
/// a line is always a header: the lines of a hunk begin with ` `, `+`, `-` or `\`.
fn split_files(text: &str) -> Vec<FileStats> {
    let mut files: Vec<FileStats> = Vec::new();
    let mut in_hunk = false;
    for line in text.lines() {
        if let Some(names) = line.strip_prefix("diff --git ") {
            files.push(FileStats {
                path: path_of(names),
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
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
        }
    }
    files
}

/// The new path of `a/<old> b/<new>`.
fn path_of(names: &str) -> String {
    names
        .rsplit_once(" b/")
        .map_or(names, |(_, new)| new)
        .to_owned()
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
            json!({"path": "dir/b c.txt", "added": 1, "removed": 0, "binary": false})
        );
        assert_eq!(summary.detail["files"][2]["binary"], true);
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
