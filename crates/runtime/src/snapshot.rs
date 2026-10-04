//! Workspace snapshots stored outside the user's repository metadata.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod benchmark;
mod history;
mod service;
pub use history::SnapshotHistory;
pub use service::{SnapshotService, WorkspaceSnapshotGuard, WorkspaceSnapshots};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn from_hex(value: String) -> Result<Self, SnapshotError> {
        if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(SnapshotError::Git("invalid snapshot object id".into()));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot io: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot git: {0}")]
    Git(String),
    #[error("snapshot store must be outside the workspace and owned by this workspace")]
    InvalidStore,
}

#[derive(Debug)]
pub struct SnapshotStore {
    root: PathBuf,
    git_dir: PathBuf,
}

impl SnapshotStore {
    pub fn open(root: &Path, directory: &Path) -> Result<Self, SnapshotError> {
        let root = root.canonicalize()?;
        let parent = directory
            .parent()
            .ok_or(SnapshotError::InvalidStore)?
            .canonicalize()?;
        let name = directory.file_name().ok_or(SnapshotError::InvalidStore)?;
        let directory = parent.join(name);
        if directory.starts_with(&root) || root.starts_with(&directory) {
            return Err(SnapshotError::InvalidStore);
        }
        let marker = directory.join("evorch-workspace");
        if directory.exists() {
            if directory.is_symlink()
                || std::fs::read(&marker)? != root.as_os_str().as_encoded_bytes()
            {
                return Err(SnapshotError::InvalidStore);
            }
        } else {
            std::fs::create_dir(&directory)?;
            std::fs::write(&marker, root.as_os_str().as_encoded_bytes())?;
        }
        let store = Self {
            root,
            git_dir: directory.join("repository"),
        };
        if store.git_dir.is_symlink() {
            return Err(SnapshotError::InvalidStore);
        }
        if !store.git_dir.exists() {
            store.git(&["init", "--quiet"])?;
        }
        Ok(store)
    }

    pub fn capture(&mut self) -> Result<SnapshotId, SnapshotError> {
        self.git(&["add", "--all", "--", "."])?;
        let output = self.git(&["write-tree"])?;
        Ok(SnapshotId(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ))
    }

    pub fn restore(&mut self, snapshot: &SnapshotId) -> Result<(), SnapshotError> {
        // Refresh only our index so files created since the snapshot are removed on restore.
        self.capture()?;
        self.git(&["read-tree", "--reset", "-u", snapshot.as_str()])?;
        Ok(())
    }

    pub fn diff(&self, before: &SnapshotId, after: &SnapshotId) -> Result<String, SnapshotError> {
        let before_tree = self.file_tree(before)?;
        let after_tree = self.file_tree(after)?;
        let benchmark = &before_tree != before || &after_tree != after;
        let arguments = [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            before_tree.as_str(),
            after_tree.as_str(),
            "--",
        ];
        let output = if benchmark {
            self.benchmark_git(&arguments, None)?
        } else {
            self.git(&arguments)?
        };
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn git(&self, arguments: &[&str]) -> Result<Output, SnapshotError> {
        self.git_input(arguments, None)
    }

    fn git_input(&self, arguments: &[&str], input: Option<&[u8]>) -> Result<Output, SnapshotError> {
        self.git_options(arguments, input, false)
    }

    fn benchmark_git(
        &self,
        arguments: &[&str],
        input: Option<&[u8]>,
    ) -> Result<Output, SnapshotError> {
        self.git_options(arguments, input, true)
    }

    fn git_options(
        &self,
        arguments: &[&str],
        input: Option<&[u8]>,
        benchmark: bool,
    ) -> Result<Output, SnapshotError> {
        let mut command = Command::new("git");
        // Inherited GIT_INDEX_FILE/GIT_COMMON_DIR must never redirect writes to user metadata.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.root)
            .args(["-c", "core.hooksPath=/dev/null"])
            .current_dir(&self.root);
        if benchmark {
            command.env("GIT_ATTR_NOSYSTEM", "1").args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.attributesFile=/dev/null",
            ]);
        }
        command.args(arguments);
        let output = if let Some(input) = input {
            use std::io::Write;
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            child.stdin.take().expect("piped stdin").write_all(input)?;
            child.wait_with_output()?
        } else {
            command.output()?
        };
        if output.status.success() {
            Ok(output)
        } else {
            Err(SnapshotError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }
}

#[cfg(test)]
mod tests;
