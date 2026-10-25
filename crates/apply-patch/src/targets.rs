//! Lists the filesystem paths a patch would touch, before anything is applied.
//!
//! Permission checks and previews need the target paths of a patch without
//! reading or writing files. Symlink handling is left to the executor that
//! applies the patch.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::error::ParseError;
use crate::error::TargetsError;
use crate::parser::Hunk;
use crate::parser::parse_patch;

/// One path touched by a patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchTarget {
    /// The path exactly as written in the patch.
    pub raw: PathBuf,
    /// `raw` if it is absolute, otherwise `root` joined with `raw`, with `.`
    /// components and repeated separators removed. `..` components never
    /// appear here because [`targets`] rejects them.
    pub resolved: PathBuf,
    pub kind: TargetKind,
}

/// How a patch touches a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetKind {
    /// `*** Add File:` creates or overwrites the path.
    Add,
    /// `*** Update File:` without `*** Move to:` edits the path in place.
    Update,
    /// `*** Delete File:` removes the path.
    Delete,
    /// `*** Update File:` with `*** Move to:` reads and then removes the path.
    MoveSource,
    /// The `*** Move to:` path receives the updated contents.
    MoveDest,
}

/// Lists every path the patch touches, in patch order, resolved against `root`.
///
/// A move contributes both its source and its destination. Relative paths need
/// `root`; the process working directory is never used. Any path containing a
/// `..` component is rejected, whether relative or absolute, and that check
/// covers every path before any path is resolved.
pub fn targets(patch: &str, root: Option<&Path>) -> Result<Vec<PatchTarget>, TargetsError> {
    let raw_targets = targets_raw(patch)?;
    if let Some((raw, _)) = raw_targets.iter().find(|(raw, _)| has_parent_dir(raw)) {
        return Err(TargetsError::ParentDir(raw.clone()));
    }
    raw_targets
        .into_iter()
        .map(|(raw, kind)| {
            let resolved = resolve(&raw, root)?;
            Ok(PatchTarget {
                raw,
                resolved,
                kind,
            })
        })
        .collect()
}

/// Lists every path the patch touches, in patch order, exactly as written.
///
/// Only parses the patch: paths are neither resolved nor checked for `..`.
pub fn targets_raw(patch: &str) -> Result<Vec<(PathBuf, TargetKind)>, ParseError> {
    let mut raw = Vec::new();
    for hunk in parse_patch(patch)?.hunks {
        match hunk {
            Hunk::AddFile { path, .. } => raw.push((path, TargetKind::Add)),
            Hunk::DeleteFile { path } => raw.push((path, TargetKind::Delete)),
            Hunk::UpdateFile {
                path,
                move_path: None,
                ..
            } => raw.push((path, TargetKind::Update)),
            Hunk::UpdateFile {
                path,
                move_path: Some(dest),
                ..
            } => {
                raw.push((path, TargetKind::MoveSource));
                raw.push((dest, TargetKind::MoveDest));
            }
        }
    }
    Ok(raw)
}

fn has_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn resolve(raw: &Path, root: Option<&Path>) -> Result<PathBuf, TargetsError> {
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        let Some(root) = root else {
            return Err(TargetsError::RelativeWithoutRoot(raw.to_path_buf()));
        };
        root.join(raw)
    };
    // Rebuilding from components drops `.` and repeated separators, so the
    // same file always yields the same string for path-based permission rules.
    Ok(joined.components().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join("workspace")
    }

    fn target(raw: &str, resolved: PathBuf, kind: TargetKind) -> PatchTarget {
        PatchTarget {
            raw: PathBuf::from(raw),
            resolved,
            kind,
        }
    }

    const ALL_KINDS_PATCH: &str = "\
*** Begin Patch
*** Add File: added.txt
+hello
*** Delete File: deleted.txt
*** Update File: src/updated.rs
@@
-old
+new
*** Update File: src/old_name.rs
*** Move to: src/new_name.rs
@@ fn main()
-    old();
+    new();
*** End Patch";

    #[test]
    fn targets_list_every_kind_in_patch_order() {
        let root = root();
        assert_eq!(
            targets(ALL_KINDS_PATCH, Some(&root)),
            Ok(vec![
                target("added.txt", root.join("added.txt"), TargetKind::Add),
                target("deleted.txt", root.join("deleted.txt"), TargetKind::Delete),
                target(
                    "src/updated.rs",
                    root.join("src/updated.rs"),
                    TargetKind::Update
                ),
                target(
                    "src/old_name.rs",
                    root.join("src/old_name.rs"),
                    TargetKind::MoveSource
                ),
                target(
                    "src/new_name.rs",
                    root.join("src/new_name.rs"),
                    TargetKind::MoveDest
                ),
            ])
        );
    }

    #[test]
    fn targets_keep_absolute_paths_as_written() {
        let elsewhere = std::env::temp_dir().join("elsewhere");
        let add = elsewhere.join("add.txt");
        let source = elsewhere.join("source.rs");
        let dest = elsewhere.join("dest.rs");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+x\n*** Update File: {}\n*** Move to: {}\n@@\n-a\n+b\n*** End Patch",
            add.display(),
            source.display(),
            dest.display()
        );
        let expected = vec![
            PatchTarget {
                raw: add.clone(),
                resolved: add.clone(),
                kind: TargetKind::Add,
            },
            PatchTarget {
                raw: source.clone(),
                resolved: source.clone(),
                kind: TargetKind::MoveSource,
            },
            PatchTarget {
                raw: dest.clone(),
                resolved: dest.clone(),
                kind: TargetKind::MoveDest,
            },
        ];

        assert_eq!(targets(&patch, None), Ok(expected.clone()));
        assert_eq!(targets(&patch, Some(&root())), Ok(expected));
    }

    #[test]
    fn targets_reject_relative_paths_without_root() {
        assert_eq!(
            targets(ALL_KINDS_PATCH, None),
            Err(TargetsError::RelativeWithoutRoot(PathBuf::from(
                "added.txt"
            )))
        );

        let move_dest = format!(
            "*** Begin Patch\n*** Update File: {}\n*** Move to: relative.rs\n@@\n-a\n+b\n*** End Patch",
            std::env::temp_dir().join("absolute.rs").display()
        );
        assert_eq!(
            targets(&move_dest, None),
            Err(TargetsError::RelativeWithoutRoot(PathBuf::from(
                "relative.rs"
            )))
        );
    }

    #[test]
    fn targets_reject_parent_dir_components() {
        let root = root();
        let absolute_escape = std::env::temp_dir().join("workspace/../outside.txt");
        for (patch, raw) in [
            (
                "*** Begin Patch\n*** Add File: ../outside.txt\n+x\n*** End Patch".to_string(),
                PathBuf::from("../outside.txt"),
            ),
            (
                "*** Begin Patch\n*** Delete File: src/../../outside.txt\n*** End Patch"
                    .to_string(),
                PathBuf::from("src/../../outside.txt"),
            ),
            (
                "*** Begin Patch\n*** Update File: src/a/../b.rs\n@@\n-a\n+b\n*** End Patch"
                    .to_string(),
                PathBuf::from("src/a/../b.rs"),
            ),
            (
                "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: ../b.rs\n@@\n-a\n+b\n*** End Patch"
                    .to_string(),
                PathBuf::from("../b.rs"),
            ),
            (
                format!(
                    "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
                    absolute_escape.display()
                ),
                absolute_escape.clone(),
            ),
        ] {
            assert_eq!(
                targets(&patch, Some(&root)),
                Err(TargetsError::ParentDir(raw.clone())),
                "{patch}"
            );
            assert_eq!(
                targets(&patch, None),
                Err(TargetsError::ParentDir(raw)),
                "{patch}"
            );
        }
    }

    #[test]
    fn targets_report_parent_dir_before_missing_root() {
        assert_eq!(
            targets(
                "*** Begin Patch\n*** Add File: first.txt\n+x\n*** Delete File: ../second.txt\n*** End Patch",
                None
            ),
            Err(TargetsError::ParentDir(PathBuf::from("../second.txt")))
        );
    }

    #[test]
    fn targets_resolve_without_current_dir_components() {
        let root = root();
        assert_eq!(
            targets(
                "*** Begin Patch\n*** Add File: ./src/./lib.rs\n+x\n*** End Patch",
                Some(&root)
            ),
            Ok(vec![target(
                "./src/./lib.rs",
                root.join("src/lib.rs"),
                TargetKind::Add
            )])
        );
    }

    #[test]
    fn targets_report_parse_failures() {
        assert_eq!(
            targets("*** Add File: a.txt\n+x\n*** End Patch", Some(&root())),
            Err(TargetsError::Parse(ParseError::InvalidPatchError(
                "The first line of the patch must be '*** Begin Patch'".to_string()
            )))
        );
        assert_eq!(
            targets(
                "*** Begin Patch\n*** Update File: a.txt\n*** End Patch",
                Some(&root())
            ),
            Err(TargetsError::Parse(ParseError::InvalidHunkError {
                message: "Update file hunk for path 'a.txt' is empty".to_string(),
                line_number: 2,
            }))
        );
    }

    #[test]
    fn targets_accept_heredoc_wrapped_patch() {
        let root = root();
        assert_eq!(
            targets(
                "<<'EOF'\n*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\nEOF\n",
                Some(&root)
            ),
            Ok(vec![target(
                "gone.txt",
                root.join("gone.txt"),
                TargetKind::Delete
            )])
        );
    }

    #[test]
    fn targets_of_patch_without_hunks_are_empty() {
        assert_eq!(
            targets("*** Begin Patch\n*** End Patch", None),
            Ok(Vec::new())
        );
    }

    #[test]
    fn targets_raw_lists_paths_without_resolving_them() {
        assert_eq!(
            targets_raw(ALL_KINDS_PATCH),
            Ok(vec![
                (PathBuf::from("added.txt"), TargetKind::Add),
                (PathBuf::from("deleted.txt"), TargetKind::Delete),
                (PathBuf::from("src/updated.rs"), TargetKind::Update),
                (PathBuf::from("src/old_name.rs"), TargetKind::MoveSource),
                (PathBuf::from("src/new_name.rs"), TargetKind::MoveDest),
            ])
        );
        assert_eq!(
            targets_raw("*** Begin Patch\n*** Add File: ../outside.txt\n+x\n*** End Patch"),
            Ok(vec![(PathBuf::from("../outside.txt"), TargetKind::Add)])
        );
    }

    #[test]
    fn targets_raw_reports_parse_failures() {
        assert_eq!(
            targets_raw("*** Begin Patch\n*** Add File: a.txt\n+x\n"),
            Err(ParseError::InvalidPatchError(
                "The last line of the patch must be '*** End Patch'".to_string()
            ))
        );
    }
}
