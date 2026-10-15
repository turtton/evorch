//! The paths a sandboxed shell can see, shared with direct file tools (ADR 0033).
//!
//! The bwrap mounts and the file-tool path check are both derived from one
//! [`SandboxBoundary`], so a region added for one is never missing from the
//! other. Allowed directories (ADR 0032) join through
//! [`SandboxBoundary::with_allowed`] and are read again on every use, so a
//! declaration reaches running sandboxes from their next command.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

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

/// A list of directories that the host may replace while runs use it.
#[derive(Debug, Clone, Default)]
pub struct LivePaths(Arc<RwLock<Arc<[PathBuf]>>>);

impl LivePaths {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self(Arc::new(RwLock::new(paths.into())))
    }

    /// The current list; later [`LivePaths::set`] calls do not change it.
    pub fn get(&self) -> Arc<[PathBuf]> {
        Arc::clone(
            &self
                .0
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub fn set(&self, paths: Vec<PathBuf>) {
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = paths.into();
    }
}

impl PartialEq for LivePaths {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.get() == other.get()
    }
}

impl Eq for LivePaths {}

/// Allowed directories with the access the run's role grants them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedRegions {
    pub paths: LivePaths,
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
    pub(crate) system: Vec<PathBuf>,
    pub(crate) allowed: Option<AllowedRegions>,
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
            system: DEFAULT_RO_BINDS.into_iter().map(PathBuf::from).collect(),
            allowed: None,
            read_only: Vec::new(),
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

    /// Allowed directories, read at each use with one access for all of them.
    #[must_use]
    pub fn with_allowed(mut self, paths: LivePaths, access: PathAccess) -> Self {
        self.allowed = Some(AllowedRegions { paths, access });
        self
    }

    pub fn allowed(&self) -> Option<&AllowedRegions> {
        self.allowed.as_ref()
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
    /// Allowed directories come right after the system paths, so a parent
    /// directory never hides the scratch, output, caches or project root.
    pub fn regions(&self) -> Vec<BoundaryRegion> {
        let region = |path: &Path, access| BoundaryRegion {
            path: path.to_path_buf(),
            access,
        };
        let mut regions = Vec::new();
        regions.extend(
            self.system
                .iter()
                .map(|path| region(path, PathAccess::ReadOnly)),
        );
        if let Some(allowed) = &self.allowed {
            regions.extend(
                allowed
                    .paths
                    .get()
                    .iter()
                    .map(|path| region(path, allowed.access)),
            );
        }
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

    // Given: allowed directories shared with the host, one of them a parent of the runtime's paths
    // When: the host replaces the list after the boundary was built
    // Then: regions list the current directories right after the system paths, before the
    // scratch, output, caches and project root that must win over them
    #[test]
    fn allowed_directories_are_read_live_and_precede_runtime_regions() {
        let allowed = LivePaths::new(vec![PathBuf::from("/var")]);
        let boundary = SandboxBoundary::new(PathBuf::from("/var/project"))
            .with_allowed(allowed.clone(), PathAccess::ReadWrite)
            .read_only("/var/tmp/out")
            .with_scratch("/var/tmp/scratch/t-1");
        let index = |regions: &[BoundaryRegion], path: &str| {
            regions
                .iter()
                .position(|region| region.path == Path::new(path))
                .unwrap_or_else(|| panic!("{path} is not listed"))
        };
        let regions = boundary.regions();
        let allowed_at = index(&regions, "/var");
        assert_eq!(regions[allowed_at].access, PathAccess::ReadWrite);
        assert!(index(&regions, "/nix") < allowed_at);
        for later in ["/var/tmp/scratch/t-1", "/var/tmp/out", "/var/project"] {
            assert!(allowed_at < index(&regions, later), "{later}");
        }

        allowed.set(vec![PathBuf::from("/srv/reference")]);

        let regions = boundary.regions();
        assert!(
            !regions
                .iter()
                .any(|region| region.path == Path::new("/var"))
        );
        assert_eq!(
            regions[index(&regions, "/srv/reference")].access,
            PathAccess::ReadWrite
        );
    }
}
