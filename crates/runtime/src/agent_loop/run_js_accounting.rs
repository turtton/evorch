//! How the calls a `run_js` program made count toward the run's budgets (ADR 0039:
//! calls inside the program count like calls made directly, with these adjustments
//! so that a program working through many files is not mistaken for a stalled loop).
//!
//! - `max_tool_calls`: a cheap read-only call (read, grep, git_diff) is a
//!   [`CallWeight::Light`] call, one tool call per ten; edit, write and shell count
//!   in full. A page that `run_js` offered as the `next` of an earlier result and the
//!   program asked for the first time is part of the read before it and counts for
//!   nothing; a page the program named on its own is an ordinary read.
//! - `max_file_rereads` guards against a model reading the same file again and
//!   again. Inside one program a file read again after something may have changed it
//!   (any shell command, or an edit or write of that file, that actually ran earlier
//!   in the same program) is a new read, not a repeat; so is a later page of a read. Files under
//!   the output directory (artifacts of cut-off output) are continuations of a call,
//!   not files the program re-reads.
//!
//! Everything is decided from the host's own record of the program's calls, in the
//! order the program made them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::run_js_host::ChildRecord;
use crate::escalation::detector::CallWeight;

/// What one child call adds to the budgets.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Accounted {
    pub weight: CallWeight,
    /// A read that counts toward `max_file_rereads` (the first read of a path counts
    /// too: the limit applies to the reads after it).
    pub counts_as_read: bool,
}

/// Accounting for the children of one `run_js` call.
pub(super) struct ChildAccounting {
    /// Sequence number of the latest read of each path in this program.
    last_read: HashMap<PathBuf, u32>,
    last_write: HashMap<PathBuf, u32>,
    last_shell: Option<u32>,
    output_root: Option<PathBuf>,
}

impl ChildAccounting {
    pub(super) fn new() -> Self {
        Self::with_output_root(tools::output::output_root().ok())
    }

    pub(super) fn with_output_root(output_root: Option<PathBuf>) -> Self {
        Self {
            last_read: HashMap::new(),
            last_write: HashMap::new(),
            last_shell: None,
            output_root,
        }
    }

    /// Accounts for `child`; children come in the order the program made them.
    pub(super) fn observe(&mut self, child: &ChildRecord) -> Accounted {
        let seq = child.seq;
        match child.name.as_str() {
            "read" if child.continuation => Accounted {
                weight: CallWeight::Free,
                counts_as_read: false,
            },
            "read" => {
                let counts_as_read = !child.is_error
                    && child
                        .paths
                        .first()
                        .is_some_and(|path| self.counts(path, seq));
                if !child.is_error
                    && let Some(path) = child.paths.first()
                {
                    self.last_read.insert(path.clone(), seq);
                }
                Accounted {
                    weight: CallWeight::Light,
                    counts_as_read,
                }
            }
            "grep" | "git_diff" => Accounted {
                weight: CallWeight::Light,
                counts_as_read: false,
            },
            "write" | "edit" => {
                // A call that was refused changed nothing; one that ran and failed may have.
                if child.executed {
                    for path in &child.paths {
                        self.last_write.insert(path.clone(), seq);
                    }
                }
                Accounted {
                    weight: CallWeight::Full,
                    counts_as_read: false,
                }
            }
            "shell" => {
                if child.executed {
                    self.last_shell = Some(seq);
                }
                Accounted {
                    weight: CallWeight::Full,
                    counts_as_read: false,
                }
            }
            _ => Accounted {
                weight: CallWeight::Full,
                counts_as_read: false,
            },
        }
    }

    /// Whether a read of `path` at `seq` counts toward `max_file_rereads`.
    fn counts(&self, path: &Path, seq: u32) -> bool {
        if self
            .output_root
            .as_deref()
            .is_some_and(|root| path.starts_with(root))
        {
            return false;
        }
        let Some(previous) = self.last_read.get(path).copied() else {
            return true;
        };
        let changed = self.last_shell.is_some_and(|shell| shell > previous)
            || self
                .last_write
                .get(path)
                .is_some_and(|write| *write > previous);
        // `seq` only orders; a read after `previous` with nothing in between is a repeat.
        seq > previous && !changed
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn child(seq: u32, name: &str, input: Value, path: Option<&str>) -> ChildRecord {
        ChildRecord {
            seq,
            name: name.into(),
            input,
            is_error: false,
            paths: path.map(PathBuf::from).into_iter().collect(),
            continuation: false,
            executed: true,
        }
    }

    fn read(seq: u32, path: &str) -> ChildRecord {
        child(seq, "read", json!({"path": path}), Some(path))
    }

    fn accounting() -> ChildAccounting {
        ChildAccounting::with_output_root(Some("/var/tmp/evorch-output-1000".into()))
    }

    fn reads(accounting: &mut ChildAccounting, children: &[ChildRecord]) -> Vec<bool> {
        children
            .iter()
            .map(|child| accounting.observe(child).counts_as_read)
            .collect()
    }

    #[test]
    fn reads_of_other_files_and_an_unchanged_repeat_are_passed_to_the_per_path_counter() {
        let mut accounting = accounting();
        let counted = reads(
            &mut accounting,
            &[read(1, "/w/a"), read(2, "/w/b"), read(3, "/w/a")],
        );
        assert_eq!(counted, [true, true, true]);
    }

    #[test]
    fn a_read_after_a_shell_command_or_a_write_of_the_file_is_not_a_repeat() {
        let mut accounting = accounting();
        let counted = reads(
            &mut accounting,
            &[
                read(1, "/tmp/scratch"),
                child(
                    2,
                    "shell",
                    json!({"command": "git show HEAD:a > /tmp/scratch"}),
                    None,
                ),
                read(3, "/tmp/scratch"),
                child(
                    4,
                    "write",
                    json!({"path": "/w/a", "content": "x"}),
                    Some("/w/a"),
                ),
                read(5, "/w/a"),
                read(6, "/w/a"),
                child(7, "edit", json!({"path": "/w/other"}), Some("/w/other")),
                read(8, "/w/a"),
            ],
        );
        // 1 first read; 3 after a shell command that may have rewritten the file; 5 the
        // first read of /w/a in this program; 6 nothing changed it since 5 (a repeat);
        // 8 only another file was edited in between (a repeat).
        assert_eq!(
            counted,
            [true, false, false, false, true, true, false, true]
        );
    }

    #[test]
    fn a_page_run_js_offered_is_part_of_the_read_before_it_and_one_the_program_named_is_not() {
        let mut accounting = accounting();
        let first = accounting.observe(&read(1, "/w/long"));
        let mut offered = child(
            2,
            "read",
            json!({"path": "/w/long", "offset": 301, "byte_offset": 0}),
            Some("/w/long"),
        );
        offered.continuation = true;
        let page = accounting.observe(&offered);
        // The same arguments again, which `run_js` did not call a continuation, and an
        // offset out of nowhere: ordinary reads.
        let again = accounting.observe(&child(
            3,
            "read",
            json!({"path": "/w/long", "offset": 301}),
            Some("/w/long"),
        ));
        let invented = accounting.observe(&child(
            4,
            "read",
            json!({"path": "/w/other", "offset": 2}),
            Some("/w/other"),
        ));
        assert_eq!(
            first,
            Accounted {
                weight: CallWeight::Light,
                counts_as_read: true
            }
        );
        assert_eq!(
            page,
            Accounted {
                weight: CallWeight::Free,
                counts_as_read: false
            }
        );
        assert_eq!(
            again,
            Accounted {
                weight: CallWeight::Light,
                counts_as_read: true
            }
        );
        assert_eq!(
            invented,
            Accounted {
                weight: CallWeight::Light,
                counts_as_read: true
            }
        );
    }

    #[test]
    fn a_call_that_never_ran_does_not_make_a_file_look_changed() {
        let mut accounting = accounting();
        let mut refused_shell = child(2, "shell", json!({"command": "echo x > /w/a"}), None);
        refused_shell.is_error = true;
        refused_shell.executed = false;
        let mut refused_write = child(3, "write", json!({"path": "/w/a"}), Some("/w/a"));
        refused_write.is_error = true;
        refused_write.executed = false;
        let mut failed_shell = child(5, "shell", json!({"command": "false"}), None);
        failed_shell.is_error = true;
        let counted = reads(
            &mut accounting,
            &[
                read(1, "/w/a"),
                refused_shell,
                refused_write,
                read(4, "/w/a"),
            ],
        );
        // Nothing ran between the two reads: the second is a repeat.
        assert_eq!(counted, [true, false, false, true]);
        let after = reads(&mut accounting, &[failed_shell, read(6, "/w/a")]);
        // A command that ran and failed may still have changed the file.
        assert_eq!(after, [false, false]);
    }

    #[test]
    fn artifacts_of_cut_off_output_are_not_files_the_program_rereads() {
        let mut accounting = accounting();
        let artifact = "/var/tmp/evorch-output-1000/3/abc.txt";
        let counted = reads(&mut accounting, &[read(1, artifact), read(2, artifact)]);
        assert_eq!(counted, [false, false]);
    }

    #[test]
    fn weights_follow_what_a_call_costs() {
        let mut accounting = accounting();
        let weight = |accounting: &mut ChildAccounting, name: &str| {
            accounting.observe(&child(1, name, json!({}), None)).weight
        };
        assert_eq!(weight(&mut accounting, "read"), CallWeight::Light);
        assert_eq!(weight(&mut accounting, "grep"), CallWeight::Light);
        assert_eq!(weight(&mut accounting, "git_diff"), CallWeight::Light);
        assert_eq!(weight(&mut accounting, "edit"), CallWeight::Full);
        assert_eq!(weight(&mut accounting, "write"), CallWeight::Full);
        assert_eq!(weight(&mut accounting, "shell"), CallWeight::Full);
    }

    #[test]
    fn a_failed_read_is_not_counted_as_a_read() {
        let mut accounting = accounting();
        let mut failed = read(1, "/w/missing");
        failed.is_error = true;
        assert!(!accounting.observe(&failed).counts_as_read);
        assert!(accounting.observe(&read(2, "/w/missing")).counts_as_read);
    }
}
