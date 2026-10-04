//! Benchmark snapshots preserve bytes, directory names and Unix permission modes.

use super::{SnapshotError, SnapshotId, SnapshotStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

const MANIFEST: &str = "evorch-benchmark-manifest-v1.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceManifest {
    version: u32,
    directories: BTreeSet<PathBuf>,
    root_mode: Option<u32>,
    modes: BTreeMap<PathBuf, u32>,
}

impl SnapshotStore {
    /// Capture ignored files, empty directories and Unix file/directory/root modes.
    /// Special files, `.git` and `.gitattributes` are unsupported and fail before
    /// capture. Ownership and extended attributes are outside this contract.
    /// The wrapper ID changes for directory-only or mode-only changes as well.
    pub fn capture_all(&mut self) -> Result<SnapshotId, SnapshotError> {
        let manifest = self.workspace_manifest()?;
        self.benchmark_git(&["add", "--force", "--all", "--", "."], None)?;
        let files = object_id(self.benchmark_git(&["write-tree"], None)?)?;
        let json = serde_json::to_vec(&manifest).map_err(invalid_manifest)?;
        let directories =
            object_id(self.benchmark_git(&["hash-object", "-w", "--stdin"], Some(&json))?)?;
        let entries = format!(
            "100644 blob {}\t{MANIFEST}\n040000 tree {}\tfiles\n",
            directories.as_str(),
            files.as_str(),
        );
        object_id(self.benchmark_git(&["mktree"], Some(entries.as_bytes()))?)
    }

    /// Restore a benchmark snapshot. Ordinary snapshots intentionally lack the
    /// complete directory manifest and are rejected here rather than approximated.
    pub fn restore_all(&mut self, snapshot: &SnapshotId) -> Result<(), SnapshotError> {
        // Git silently omits special files, and attributes can transform bytes
        // or execute filters. Reject both before refreshing the private index.
        self.workspace_manifest()?;
        let (files, manifest) = self
            .benchmark_tree(snapshot)?
            .ok_or_else(|| invalid_manifest("missing benchmark directory manifest"))?;
        // Refresh our private index so Git removes files introduced after capture.
        self.benchmark_git(&["add", "--force", "--all", "--", "."], None)?;
        self.benchmark_git(&["read-tree", "--reset", "-u", files.as_str()], None)?;
        let current = self.workspace_manifest()?.directories;
        let mut extra: Vec<_> = current.difference(&manifest.directories).collect();
        extra.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        for path in extra {
            // Never recursively delete unexpected contents. A remaining nonempty
            // directory means restoration was incomplete and must fail closed.
            std::fs::remove_dir(self.root.join(path))?;
        }
        for path in &manifest.directories {
            std::fs::create_dir_all(self.root.join(path))?;
        }
        self.restore_modes(&manifest)?;
        Ok(())
    }

    pub(super) fn file_tree(&self, snapshot: &SnapshotId) -> Result<SnapshotId, SnapshotError> {
        Ok(self
            .benchmark_tree(snapshot)?
            .map_or_else(|| snapshot.clone(), |(files, _)| files))
    }

    fn benchmark_tree(
        &self,
        snapshot: &SnapshotId,
    ) -> Result<Option<(SnapshotId, WorkspaceManifest)>, SnapshotError> {
        let listing = self.benchmark_git(&["ls-tree", snapshot.as_str()], None)?;
        let listing = String::from_utf8_lossy(&listing.stdout);
        let lines: Vec<_> = listing.lines().collect();
        if lines.len() != 2 {
            return Ok(None);
        }
        let Some(blob) = lines[0]
            .strip_prefix("100644 blob ")
            .and_then(|line| line.strip_suffix(&format!("\t{MANIFEST}")))
        else {
            return Ok(None);
        };
        let Some(files) = lines[1]
            .strip_prefix("040000 tree ")
            .and_then(|line| line.strip_suffix("\tfiles"))
        else {
            return Ok(None);
        };
        let blob = SnapshotId::from_hex(blob.to_owned())?;
        let files = SnapshotId::from_hex(files.to_owned())?;
        let json = self.benchmark_git(&["cat-file", "blob", blob.as_str()], None)?;
        let manifest: WorkspaceManifest =
            serde_json::from_slice(&json.stdout).map_err(invalid_manifest)?;
        if manifest.version != 1
            || manifest.root_mode.is_some_and(|mode| mode & !0o7777 != 0)
            || manifest.modes.values().any(|mode| mode & !0o7777 != 0)
            || manifest
                .directories
                .iter()
                .chain(manifest.modes.keys())
                .any(|path| {
                path.as_os_str().is_empty()
                    || path.components().any(|component| {
                        !matches!(component, Component::Normal(name) if name != ".git" && name != ".gitattributes")
                    })
            })
        {
            return Err(invalid_manifest(
                "unsupported version or unsafe directory path",
            ));
        }
        Ok(Some((files, manifest)))
    }

    fn workspace_manifest(&self) -> Result<WorkspaceManifest, SnapshotError> {
        fn visit(
            root: &Path,
            relative: &Path,
            manifest: &mut WorkspaceManifest,
        ) -> Result<(), SnapshotError> {
            for entry in std::fs::read_dir(root.join(relative))? {
                let entry = entry?;
                let path = relative.join(entry.file_name());
                if entry.file_name() == ".git" || entry.file_name() == ".gitattributes" {
                    return Err(unsupported_entry(
                        &path,
                        "Git metadata and attributes are unsupported",
                    ));
                }
                let kind = entry.file_type()?;
                if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() {
                    return Err(unsupported_entry(
                        &path,
                        "special files cannot be captured by Git",
                    ));
                }
                // Git preserves symlink targets; never traverse or chmod them.
                if !kind.is_symlink()
                    && let Some(mode) = unix_mode(&entry.metadata()?)
                {
                    manifest.modes.insert(path.clone(), mode);
                }
                if kind.is_dir() {
                    manifest.directories.insert(path.clone());
                    visit(root, &path, manifest)?;
                }
            }
            Ok(())
        }
        let mut manifest = WorkspaceManifest {
            version: 1,
            directories: BTreeSet::new(),
            root_mode: unix_mode(&std::fs::metadata(&self.root)?),
            modes: BTreeMap::new(),
        };
        visit(&self.root, Path::new(""), &mut manifest)?;
        Ok(manifest)
    }

    fn restore_modes(&self, manifest: &WorkspaceManifest) -> Result<(), SnapshotError> {
        // Children first: a recorded directory mode may make its descendants
        // inaccessible once restored. Root permissions are restored last.
        let mut paths: Vec<_> = manifest.modes.iter().collect();
        paths.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
        for (relative, mode) in paths {
            let path = self.root.join(relative);
            let kind = std::fs::symlink_metadata(&path)?.file_type();
            if kind.is_symlink()
                || kind.is_dir() != manifest.directories.contains(relative)
                || (!kind.is_dir() && !kind.is_file())
            {
                return Err(unsupported_entry(
                    relative,
                    "restored entry type differs from manifest",
                ));
            }
            set_unix_mode(&path, *mode)?;
        }
        if let Some(mode) = manifest.root_mode {
            set_unix_mode(&self.root, mode)?;
        }
        Ok(())
    }
}

fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn set_unix_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn unsupported_entry(path: &Path, reason: &str) -> SnapshotError {
    SnapshotError::Git(format!(
        "unsupported benchmark entry {}: {reason}",
        path.display()
    ))
}

fn object_id(output: std::process::Output) -> Result<SnapshotId, SnapshotError> {
    SnapshotId::from_hex(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn invalid_manifest(error: impl std::fmt::Display) -> SnapshotError {
    SnapshotError::Git(format!("invalid benchmark workspace manifest: {error}"))
}
