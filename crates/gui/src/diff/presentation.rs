//! Presentation-only unified diff parsing. Never executes Git or changes files.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Addition,
    Deletion,
    Hunk,
    Metadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffFile {
    pub path: String,
    pub status: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Default)]
pub struct DiffDocument {
    pub files: Vec<DiffFile>,
}

impl DiffDocument {
    pub fn parse(text: &str) -> Self {
        let mut document = Self::default();
        let mut old = 0;
        let mut new = 0;
        let mut in_hunk = false;
        for line in text.lines() {
            if let Some(paths) = line.strip_prefix("diff --git ") {
                let paths = git_paths(paths);
                document.files.push(DiffFile {
                    path: paths
                        .last()
                        .cloned()
                        .unwrap_or_else(|| "Changed file".into()),
                    status: "Modified",
                    ..DiffFile::default()
                });
                in_hunk = false;
                continue;
            }
            let Some(file) = document.files.last_mut() else {
                continue;
            };
            if line.starts_with("@@") {
                let starts = hunk_starts(line);
                in_hunk = starts.is_some();
                if let Some((old_start, new_start)) = starts {
                    old = old_start;
                    new = new_start;
                }
                file.lines.push(DiffLine {
                    kind: LineKind::Hunk,
                    old: None,
                    new: None,
                    text: line.into(),
                });
            } else if in_hunk && line.starts_with('+') {
                file.additions += 1;
                file.lines.push(DiffLine {
                    kind: LineKind::Addition,
                    old: None,
                    new: Some(new),
                    text: line[1..].into(),
                });
                new = new.saturating_add(1);
            } else if in_hunk && line.starts_with('-') {
                file.deletions += 1;
                file.lines.push(DiffLine {
                    kind: LineKind::Deletion,
                    old: Some(old),
                    new: None,
                    text: line[1..].into(),
                });
                old = old.saturating_add(1);
            } else if in_hunk && line.starts_with(' ') {
                file.lines.push(DiffLine {
                    kind: LineKind::Context,
                    old: Some(old),
                    new: Some(new),
                    text: line[1..].into(),
                });
                old = old.saturating_add(1);
                new = new.saturating_add(1);
            } else if !in_hunk && line.starts_with("new file mode ") {
                file.status = "Added";
            } else if !in_hunk && line.starts_with("deleted file mode ") {
                file.status = "Deleted";
            } else if !in_hunk && line.starts_with("rename from ") {
                file.status = "Renamed";
                metadata(file, line);
            } else if !in_hunk && line.starts_with("rename to ") {
                file.path = decode_path(&line[10..]);
                metadata(file, line);
            } else if !in_hunk && line.starts_with("+++ ") {
                let path = decode_path(&line[4..]);
                if path != "/dev/null" {
                    file.path = strip_side(&path).into();
                }
            } else if !in_hunk && line.starts_with("--- ") {
                if file.status == "Deleted" {
                    file.path = strip_side(&decode_path(&line[4..])).into();
                }
            } else if !line.starts_with("index ") {
                metadata(file, line);
            }
        }
        document
    }

    pub fn additions(&self) -> usize {
        self.files.iter().map(|file| file.additions).sum()
    }

    pub fn deletions(&self) -> usize {
        self.files.iter().map(|file| file.deletions).sum()
    }
}

fn metadata(file: &mut DiffFile, text: &str) {
    file.lines.push(DiffLine {
        kind: LineKind::Metadata,
        old: None,
        new: None,
        text: text.into(),
    });
}

fn hunk_starts(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.strip_prefix("@@ ")?.split_whitespace();
    let start = |field: &str, sign| {
        field
            .strip_prefix(sign)?
            .split(',')
            .next()?
            .parse::<usize>()
            .ok()
    };
    let old = start(fields.next()?, '-')?;
    let new = start(fields.next()?, '+')?;
    (fields.next()? == "@@").then_some((old, new))
}

fn strip_side(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
}

// Git quotes paths containing tabs, quotes or non-ASCII bytes using C escapes.
fn decode_path(path: &str) -> String {
    let Some(quoted) = path.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return path.into();
    };
    let mut bytes = quoted.bytes().peekable();
    let mut output = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        match bytes.next() {
            Some(b'n') => output.push(b'\n'),
            Some(b't') => output.push(b'\t'),
            Some(b'r') => output.push(b'\r'),
            Some(first @ b'0'..=b'7') => {
                let mut value = u16::from(first - b'0');
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = bytes.peek().copied() {
                        bytes.next();
                        value = value * 8 + u16::from(next - b'0');
                    } else {
                        break;
                    }
                }
                output.push(value as u8);
            }
            Some(other) => output.push(other),
            None => output.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn git_paths(paths: &str) -> Vec<String> {
    // Unquoted paths may contain spaces. Git's side prefixes delimit them.
    if !paths.starts_with('"')
        && let Some((old, new)) = paths.rsplit_once(" b/")
    {
        return vec![strip_side(old).into(), new.into()];
    }
    let mut result = Vec::new();
    let mut rest = paths.trim();
    while !rest.is_empty() {
        let end = if rest.starts_with('"') {
            let mut escaped = false;
            rest.char_indices()
                .skip(1)
                .find_map(|(index, ch)| {
                    if ch == '"' && !escaped {
                        Some(index + 1)
                    } else {
                        escaped = ch == '\\' && !escaped;
                        None
                    }
                })
                .unwrap_or(rest.len())
        } else {
            rest.find(' ').unwrap_or(rest.len())
        };
        result.push(strip_side(&decode_path(&rest[..end])).into());
        rest = rest[end..].trim_start();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_both_line_numbers_across_hunks_without_counting_file_headers() {
        let doc = DiffDocument::parse(
            "diff --git a/app.rs b/app.rs\nindex abc..def 100644\n--- a/app.rs\n+++ b/app.rs\n@@ -3,2 +3,3 @@ fn main\n context\n-old\n+new\n+extra\n@@ -20 +21 @@\n--- content\n+++ content\n\\ No newline at end of file\n",
        );
        let file = &doc.files[0];
        assert_eq!(
            (file.path.as_str(), file.additions, file.deletions),
            ("app.rs", 3, 2)
        );
        assert_eq!((file.lines[1].old, file.lines[1].new), (Some(3), Some(3)));
        assert_eq!((file.lines[2].old, file.lines[2].new), (Some(4), None));
        assert_eq!((file.lines[4].old, file.lines[4].new), (None, Some(5)));
        assert_eq!(file.lines[6].text, "-- content");
        assert_eq!(file.lines[7].new, Some(21));
        assert_eq!(file.lines[8].kind, LineKind::Metadata);
    }

    #[test]
    fn handles_added_deleted_renamed_and_binary_files() {
        let doc = DiffDocument::parse(
            "diff --git a/new b/new\nnew file mode 100644\n--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+hello\ndiff --git a/old b/old\ndeleted file mode 100644\n--- a/old\n+++ /dev/null\n@@ -1 +0,0 @@\n-goodbye\ndiff --git a/before b/after\nsimilarity index 100%\nrename from before\nrename to after\ndiff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ\n",
        );
        assert_eq!(doc.files.len(), 4);
        assert_eq!(doc.files[0].status, "Added");
        assert_eq!(doc.files[1].status, "Deleted");
        assert_eq!(doc.files[1].path, "old");
        assert_eq!(doc.files[2].status, "Renamed");
        assert_eq!(doc.files[2].path, "after");
        assert_eq!(doc.files[3].lines[0].kind, LineKind::Metadata);
        assert_eq!((doc.additions(), doc.deletions()), (1, 1));
    }

    #[test]
    fn decodes_git_quoted_paths_and_keeps_spaces() {
        let doc = DiffDocument::parse(
            "diff --git \"a/\\346\\227\\245\\346\\234\\254.txt\" \"b/\\346\\227\\245\\346\\234\\254.txt\"\nBinary files differ\ndiff --git a/a file.txt b/a file.txt\nold mode 100644\nnew mode 100755\n",
        );
        assert_eq!(doc.files[0].path, "日本.txt");
        assert_eq!(doc.files[1].path, "a file.txt");
        assert_eq!(doc.files[1].lines.len(), 2);
    }

    #[test]
    fn plain_text_and_incomplete_hunks_are_safe() {
        assert!(
            DiffDocument::parse("first line\nsecond line")
                .files
                .is_empty()
        );
        let doc = DiffDocument::parse("diff --git a/a b/a\n@@ -bad +1 @@\n+partial");
        assert_eq!(doc.files[0].additions, 0);
        assert_eq!(doc.files[0].lines[1].text, "+partial");
    }
}
