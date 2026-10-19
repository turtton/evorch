//! What changed between two outlines of a page. Lines are matched by their
//! DOM node, which keeps its ref for as long as it exists.

use std::collections::HashMap;

use super::{Line, MAX_SNAPSHOT_LINES};

/// Changed lines, with the unchanged lines that hold them as context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub text: String,
    /// Lines added, removed or changed.
    pub changes: usize,
    /// Lines in `text`, context included.
    pub lines: usize,
}

#[derive(Clone, Copy)]
enum Entry<'a> {
    Same(&'a Line),
    Changed { old: &'a Line, new: &'a Line },
    Removed(&'a Line),
    Added(&'a Line),
}

impl Entry<'_> {
    fn depth(&self) -> usize {
        match self {
            Self::Same(line) | Self::Removed(line) | Self::Added(line) => line.depth,
            Self::Changed { new, .. } => new.depth,
        }
    }

    /// Whether the line is in the new outline, so it can hold others.
    fn stays(&self) -> bool {
        matches!(self, Self::Same(_) | Self::Changed { .. })
    }

    fn changed(&self) -> bool {
        !matches!(self, Self::Same(_))
    }
}

/// Lists how `new` differs from `old`: `+` added, `-` removed, `~` changed,
/// each under the unchanged lines that hold it.
pub fn diff(old: &[Line], new: &[Line]) -> Diff {
    let entries = align(old, new);
    let mut shown = vec![false; entries.len()];
    for (index, entry) in entries.iter().enumerate() {
        if !entry.changed() {
            continue;
        }
        shown[index] = true;
        let mut depth = entry.depth();
        for (holder, above) in entries[..index].iter().enumerate().rev() {
            if depth == 0 {
                break;
            }
            if above.stays() && above.depth() < depth {
                shown[holder] = true;
                depth = above.depth();
            }
        }
    }
    let mut out = Diff::default();
    for (entry, _) in entries.iter().zip(&shown).filter(|(_, shown)| **shown) {
        out.lines += 1;
        if entry.changed() {
            out.changes += 1;
        }
        if out.lines > MAX_SNAPSHOT_LINES {
            continue;
        }
        let line = match entry {
            Entry::Same(line) => format!("  {}", indented(line)),
            Entry::Added(line) => format!("+ {}", indented(line)),
            Entry::Removed(line) => format!("- {}", indented(line)),
            Entry::Changed { old, new } if old.text == new.text => {
                format!("~ {} (moved)", indented(new))
            }
            Entry::Changed { old, new } => format!("~ {} (was: {})", indented(new), old.text),
        };
        out.text.push_str(&line);
        out.text.push('\n');
    }
    if out.lines > MAX_SNAPSHOT_LINES {
        out.text.push_str(&format!(
            "[changes truncated: {} more lines; take a browser_snapshot]\n",
            out.lines - MAX_SNAPSHOT_LINES
        ));
    }
    out
}

fn indented(line: &Line) -> String {
    format!("{}{}", "  ".repeat(line.depth), line.text)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key<'a> {
    Node(i64),
    /// Lines without a node are told apart by their text and place among
    /// equal ones.
    Text(&'a str, usize, usize),
}

fn keys(lines: &[Line]) -> Vec<Key<'_>> {
    let mut seen: HashMap<(&str, usize), usize> = HashMap::new();
    lines
        .iter()
        .map(|line| match line.node {
            Some(node) => Key::Node(node),
            None => {
                let count = seen.entry((&line.text, line.depth)).or_default();
                *count += 1;
                Key::Text(&line.text, line.depth, *count)
            }
        })
        .collect()
}

/// Pairs the lines both outlines share, keeping their order (the longest
/// run of shared lines in the same order), and merges the rest around them.
fn align<'a>(old: &'a [Line], new: &'a [Line]) -> Vec<Entry<'a>> {
    let new_keys = keys(new);
    let mut position: HashMap<&Key, usize> = HashMap::new();
    for (index, key) in new_keys.iter().enumerate() {
        position.entry(key).or_insert(index);
    }
    let old_keys = keys(old);
    let shared: Vec<(usize, usize)> = old_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| Some((index, *position.get(key)?)))
        .collect();
    let mut paired_old = vec![None; old.len()];
    let mut paired_new = vec![false; new.len()];
    for (old_index, new_index) in increasing(&shared) {
        paired_old[old_index] = Some(new_index);
        paired_new[new_index] = true;
    }
    let (mut i, mut j) = (0, 0);
    let mut entries = Vec::with_capacity(old.len().max(new.len()));
    while i < old.len() || j < new.len() {
        if i < old.len() && paired_old[i].is_none() {
            entries.push(Entry::Removed(&old[i]));
            i += 1;
        } else if j < new.len() && !paired_new[j] {
            entries.push(Entry::Added(&new[j]));
            j += 1;
        } else {
            let (old_line, new_line) = (&old[i], &new[j]);
            entries.push(if old_line == new_line {
                Entry::Same(new_line)
            } else {
                Entry::Changed {
                    old: old_line,
                    new: new_line,
                }
            });
            i += 1;
            j += 1;
        }
    }
    entries
}

/// The longest subsequence of `pairs` (ordered by their first item) whose
/// second items increase.
fn increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; pairs.len()];
    for (index, &(_, value)) in pairs.iter().enumerate() {
        let at = tails.partition_point(|&tail| pairs[tail].1 < value);
        if at > 0 {
            previous[index] = Some(tails[at - 1]);
        }
        if at == tails.len() {
            tails.push(index);
        } else {
            tails[at] = index;
        }
    }
    let mut run = Vec::with_capacity(tails.len());
    let mut cursor = tails.last().copied();
    while let Some(index) = cursor {
        run.push(pairs[index]);
        cursor = previous[index];
    }
    run.reverse();
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(depth: usize, text: &str, node: Option<i64>) -> Line {
        Line {
            depth,
            text: text.into(),
            node,
        }
    }

    #[test]
    fn changes_show_under_the_lines_that_hold_them() {
        // Given: a page whose status text was replaced and whose button got
        // disabled, while a sibling list stayed as it was.
        let old = vec![
            line(0, "- main [ref=e1]", Some(1)),
            line(1, "- list [ref=e2]", Some(2)),
            line(2, "- listitem \"A\" [ref=e3]", Some(3)),
            line(1, "- form [ref=e4]", Some(4)),
            line(2, "- text \"Loading\"", Some(5)),
            line(2, "- button \"Buy\" [ref=e6]", Some(6)),
        ];
        let mut new = old.clone();
        new[4] = line(2, "- text \"Ready\"", Some(7));
        new[5] = line(2, "- button \"Buy\" [disabled=true] [ref=e6]", Some(6));
        new.push(line(1, "- link \"Next\" [ref=e8]", Some(8)));

        // When: the outlines are compared.
        let diff = diff(&old, &new);

        // Then: only the changes and what holds them are listed.
        assert_eq!(
            diff.text,
            "  - main [ref=e1]\n\
             \x20   - form [ref=e4]\n\
             -     - text \"Loading\"\n\
             +     - text \"Ready\"\n\
             ~     - button \"Buy\" [disabled=true] [ref=e6] (was: - button \"Buy\" [ref=e6])\n\
             +   - link \"Next\" [ref=e8]\n"
        );
        assert_eq!((diff.changes, diff.lines), (4, 6));
    }

    #[test]
    fn equal_outlines_have_no_changes_and_reordered_lines_count_once() {
        let old = vec![
            line(0, "- list [ref=e1]", Some(1)),
            line(1, "- listitem \"A\" [ref=e2]", Some(2)),
            line(1, "- listitem \"B\" [ref=e3]", Some(3)),
        ];
        let swapped = vec![old[0].clone(), old[2].clone(), old[1].clone()];

        assert_eq!(diff(&old, &old), Diff::default());
        let moved = diff(&old, &swapped);
        assert_eq!(
            moved.text,
            "  - list [ref=e1]\n-   - listitem \"A\" [ref=e2]\n+   - listitem \"A\" [ref=e2]\n"
        );
    }

    #[test]
    fn lines_without_nodes_are_matched_by_text_and_place() {
        let old = vec![line(0, "- text \"x\"", None), line(0, "- text \"x\"", None)];
        let new = vec![line(0, "- text \"x\"", None)];

        let diff = diff(&old, &new);

        assert_eq!(diff.text, "- - text \"x\"\n");
    }
}
