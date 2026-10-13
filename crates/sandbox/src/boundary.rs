//! The paths a sandboxed shell can see, shared with direct file tools (ADR 0033).
//!
//! The bwrap mounts and the file-tool path check are both derived from one
//! [`SandboxBoundary`], so a region added for one is never missing from the
//! other. Allowed directories (ADR 0032) join through
//! [`SandboxBoundary::read_only`] / [`SandboxBoundary::read_write`].

use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_RO_BINDS: [&str; 6] = ["/usr", "/bin", "/lib", "/lib64", "/etc", "/nix"];

/// Cargo cache entries visible to shells. Credentials and config stay hidden.
const CARGO_CACHES: [&str; 6] = [
    "registry/index",
    "registry/cache",
    "registry/src",
    "git/db",
    "git/checkouts",
    "bin",
];
const RUSTUP_CACHES: [&str; 2] = ["toolchains", "settings.toml"];

/// Access a region grants without escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathAccess {
    ReadOnly,
    ReadWrite,
}

/// One host path inside the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryRegion {
    pub path: PathBuf,
    pub access: PathAccess,
}

/// A read-only build cache mounted at a different path inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheMount {
    pub host: PathBuf,
    pub sandbox: PathBuf,
}

/// Read-write and read-only host paths of one run's sandbox.
#[derive(Debug, Clone)]
pub struct SandboxBoundary {
    pub(crate) workspace_root: PathBuf,
    pub(crate) read_only: Vec<PathBuf>,
    pub(crate) read_write: Vec<PathBuf>,
    pub(crate) cargo_home: Option<PathBuf>,
    pub(crate) rustup_home: Option<PathBuf>,
    pub(crate) scratch: Option<PathBuf>,
}

impl SandboxBoundary {
    /// The project root plus the default read-only system paths and build caches.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            read_only: DEFAULT_RO_BINDS.into_iter().map(PathBuf::from).collect(),
            read_write: Vec::new(),
            cargo_home: tool_home("CARGO_HOME", ".cargo"),
            rustup_home: tool_home("RUSTUP_HOME", ".rustup"),
            scratch: None,
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn scratch(&self) -> Option<&Path> {
        self.scratch.as_deref()
    }

    #[must_use]
    pub fn read_only(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_only.push(path.into());
        self
    }

    #[must_use]
    pub fn read_write(mut self, path: impl Into<PathBuf>) -> Self {
        self.read_write.push(path.into());
        self
    }

    /// The thread's `TMPDIR`, read-write at its host path.
    #[must_use]
    pub fn with_scratch(mut self, path: impl Into<PathBuf>) -> Self {
        self.scratch = Some(path.into());
        self
    }

    #[must_use]
    pub fn cargo_cache(mut self, path: impl Into<PathBuf>) -> Self {
        self.cargo_home = Some(path.into());
        self
    }

    /// Host Cargo / rustup cache paths with their in-sandbox destinations.
    pub fn cache_mounts(&self) -> Vec<CacheMount> {
        let mut mounts = Vec::new();
        if let Some(home) = &self.cargo_home {
            mounts.extend(CARGO_CACHES.iter().map(|relative| CacheMount {
                host: home.join(relative),
                sandbox: Path::new("/tmp/home/.cargo").join(relative),
            }));
        }
        if let Some(home) = &self.rustup_home {
            mounts.extend(RUSTUP_CACHES.iter().map(|relative| CacheMount {
                host: home.join(relative),
                sandbox: Path::new("/tmp/home/.rustup").join(relative),
            }));
        }
        mounts
    }

    pub(crate) fn rustup_home(&self) -> Option<&Path> {
        self.rustup_home.as_deref()
    }

    /// Host paths in bwrap mount order; a later region covering a path wins.
    pub fn regions(&self) -> Vec<BoundaryRegion> {
        let region = |path: &Path, access| BoundaryRegion {
            path: path.to_path_buf(),
            access,
        };
        let mut regions = Vec::new();
        regions.extend(
            self.scratch
                .iter()
                .map(|path| region(path, PathAccess::ReadWrite)),
        );
        regions.extend(
            self.read_only
                .iter()
                .map(|path| region(path, PathAccess::ReadOnly)),
        );
        regions.extend(
            self.cache_mounts()
                .iter()
                .map(|mount| region(&mount.host, PathAccess::ReadOnly)),
        );
        regions.extend(
            self.read_write
                .iter()
                .map(|path| region(path, PathAccess::ReadWrite)),
        );
        regions.push(region(&self.workspace_root, PathAccess::ReadWrite));
        regions
    }
}

fn tool_home(variable: &str, default: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(default)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_list_every_mount_by_host_path_in_mount_order() {
        // Given: a boundary with every kind of region.
        let boundary = SandboxBoundary::new(PathBuf::from("/project"))
            .cargo_cache("/home/u/.cargo")
            .read_only("/var/tmp/out")
            .read_write("/srv/rw")
            .with_scratch("/var/tmp/scratch/t-1");
        let boundary = SandboxBoundary {
            rustup_home: Some(PathBuf::from("/home/u/.rustup")),
            ..boundary
        };

        // When: the regions are listed.
        let regions = boundary.regions();

        // Then: caches appear by host path, and the project root comes last.
        let listed = |access| {
            regions
                .iter()
                .filter(|region| region.access == access)
                .map(|region| region.path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listed(PathAccess::ReadWrite),
            ["/var/tmp/scratch/t-1", "/srv/rw", "/project"]
        );
        let read_only = listed(PathAccess::ReadOnly);
        for path in [
            "/usr",
            "/bin",
            "/lib",
            "/lib64",
            "/etc",
            "/nix",
            "/var/tmp/out",
            "/home/u/.cargo/registry/src",
            "/home/u/.cargo/git/checkouts",
            "/home/u/.cargo/bin",
            "/home/u/.rustup/toolchains",
        ] {
            assert!(read_only.iter().any(|listed| listed == path), "{path}");
        }
        assert!(!read_only.iter().any(|path| path.starts_with("/tmp/home")));
        assert!(!read_only.iter().any(|path| path == "/home/u/.cargo"));
        assert_eq!(regions.last().unwrap().path, Path::new("/project"));
    }
}
