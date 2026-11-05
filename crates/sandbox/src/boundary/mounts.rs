//! One per-command snapshot for shell mounts and host file-tool precedence.

use super::{CacheMount, PathAccess, SandboxBoundary};
use std::path::{Path, PathBuf};

pub(crate) const SANDBOX_HOME: &str = "/tmp/home";

#[derive(Debug, Clone)]
pub(crate) struct BoundaryMount {
    pub host: PathBuf,
    pub sandbox: PathBuf,
    pub access: PathAccess,
    pub optional: bool,
    pub host_region: bool,
    pub home_alias: bool,
    pub cache: bool,
}

impl BoundaryMount {
    fn direct(path: &Path, access: PathAccess, optional: bool) -> Self {
        Self {
            host: path.into(),
            sandbox: path.into(),
            access,
            optional,
            host_region: true,
            home_alias: false,
            cache: false,
        }
    }

    fn alias(host: &Path, sandbox: &Path, access: PathAccess, optional: bool) -> Self {
        Self {
            sandbox: sandbox.into(),
            host_region: false,
            home_alias: sandbox.starts_with(SANDBOX_HOME),
            ..Self::direct(host, access, optional)
        }
    }

    pub fn flag(&self) -> &'static str {
        match (self.access, self.optional) {
            (PathAccess::ReadOnly, true) => "--ro-bind-try",
            (PathAccess::ReadOnly, false) => "--ro-bind",
            (PathAccess::ReadWrite, true) => "--bind-try",
            (PathAccess::ReadWrite, false) => "--bind",
        }
    }
}

pub(crate) struct MountPlan {
    pub mounts: Vec<BoundaryMount>,
}

impl SandboxBoundary {
    pub(crate) fn mount_plan(&self, scratch: Option<&Path>, isolate: bool) -> MountPlan {
        use PathAccess::{ReadOnly, ReadWrite};
        // Read LivePaths exactly once: originals, mirrors and cache classification
        // must all describe the same declaration, including during a concurrent set.
        let allowed = self.allowed.as_ref().map(|a| (a.paths.get(), a.access));
        let mut mirrors = Vec::new();
        if let (Some(home), Some((paths, access))) = (&self.host_home, &allowed) {
            for path in paths.iter() {
                // Declarations are canonicalized by the host. Do not re-resolve a
                // declaration into a different HOME-relative name at command time.
                if let Ok(relative) = path.strip_prefix(home)
                    && !relative.as_os_str().is_empty()
                {
                    mirrors.push(BoundaryMount::alias(
                        path,
                        &Path::new(SANDBOX_HOME).join(relative),
                        *access,
                        true,
                    ));
                }
            }
        }
        mirrors.sort_by(|a, b| a.sandbox.cmp(&b.sandbox));
        mirrors.dedup_by(|a, b| a.sandbox == b.sandbox);

        let mut caches = self.cache_mounts();
        if isolate
            && let Some(home) = &self.rustup_home
            && home.is_absolute()
        {
            let toolchains = home.join("toolchains");
            caches.insert(
                0,
                CacheMount {
                    host: toolchains.clone(),
                    sandbox: toolchains,
                },
            );
        }
        let mut overlays = Vec::new();
        let mut projections = mirrors.clone();
        let mut early = Vec::new();
        let mut late = Vec::new();
        for cache in caches {
            let host = if mirrors.is_empty() {
                cache.host.clone()
            } else {
                cache
                    .host
                    .canonicalize()
                    .unwrap_or_else(|_| cache.host.clone())
            };
            // Destination coverage replaces the automatic mount. Canonical
            // source coverage alone must retain its different Cargo/Rustup alias.
            let covered = mirrors
                .iter()
                .any(|m| cache.sandbox.starts_with(&m.sandbox))
                || (cache.host == cache.sandbox
                    && allowed.as_ref().is_some_and(|(paths, _)| {
                        paths.iter().any(|p| cache.sandbox.starts_with(p))
                    }));
            if covered {
                continue;
            }
            let source_cover = mirrors.iter().find(|m| host.starts_with(&m.host));
            let source_children: Vec<_> = mirrors
                .iter()
                .filter(|m| m.host.starts_with(&host))
                .collect();
            let ancestor = mirrors
                .iter()
                .any(|m| m.sandbox.starts_with(&cache.sandbox))
                || !source_children.is_empty()
                || (cache.host == cache.sandbox
                    && allowed.as_ref().is_some_and(|(paths, _)| {
                        paths.iter().any(|p| p.starts_with(&cache.sandbox))
                    }));
            let access = source_cover.map_or(ReadOnly, |m| m.access);
            let mount = BoundaryMount {
                host: host.clone(),
                sandbox: cache.sandbox.clone(),
                access,
                cache: true,
                home_alias: source_cover.is_some() && cache.sandbox.starts_with(SANDBOX_HOME),
                ..BoundaryMount::direct(&cache.host, ReadOnly, false)
            };
            if source_cover.is_some() {
                projections.push(BoundaryMount::alias(&host, &cache.sandbox, access, true));
                early.push(mount);
            } else if ancestor {
                early.push(mount);
                for child in source_children {
                    let relative = child.host.strip_prefix(&host).expect("cache descendant");
                    let alias = BoundaryMount::alias(
                        &child.host,
                        &cache.sandbox.join(relative),
                        child.access,
                        true,
                    );
                    overlays.push(alias.clone());
                    projections.push(alias);
                }
            } else {
                late.push(mount);
            }
        }
        overlays.retain(|alias| !shadowed_by_explicit(alias, &mirrors));
        projections.retain(|alias| !shadowed_by_explicit(alias, &mirrors));
        sort_aliases(&mut overlays);
        sort_aliases(&mut projections);

        let mut mounts: Vec<_> = self
            .system
            .iter()
            .map(|p| BoundaryMount::direct(p, ReadOnly, true))
            .collect();
        if let Some(scratch) = scratch {
            mounts.push(BoundaryMount {
                home_alias: false,
                ..BoundaryMount::alias(scratch, Path::new("/tmp"), ReadWrite, false)
            });
        }
        mounts.extend(early);
        if let Some((paths, access)) = &allowed {
            mounts.extend(
                paths
                    .iter()
                    .map(|p| BoundaryMount::direct(p, *access, true)),
            );
        }
        mounts.extend(overlays);
        // Explicit HOME paths own their identity, even when a different host
        // cache happens to map a descendant onto the same virtual subtree.
        mounts.extend(mirrors.iter().cloned());
        if let Some(scratch) = scratch {
            append_region(
                &mut mounts,
                &projections,
                &mirrors,
                scratch,
                ReadWrite,
                false,
            );
        }
        for path in self
            .read_only
            .iter()
            .map(PathBuf::as_path)
            .chain(self.current_thread_read_directories())
        {
            append_region(&mut mounts, &projections, &mirrors, path, ReadOnly, true);
        }
        mounts.extend(late);
        for path in &self.read_write {
            append_region(&mut mounts, &projections, &mirrors, path, ReadWrite, false);
        }
        append_region(
            &mut mounts,
            &projections,
            &mirrors,
            &self.workspace_root,
            ReadWrite,
            false,
        );
        MountPlan { mounts }
    }
}

fn sort_aliases(aliases: &mut Vec<BoundaryMount>) {
    aliases.sort_by(|a, b| a.sandbox.cmp(&b.sandbox));
    aliases.dedup_by(|a, b| a.sandbox == b.sandbox);
}

// A virtual path below an explicit mirror must continue to name that mirror's
// host subtree, not another allowed directory reached through a cache alias.
fn shadowed_by_explicit(alias: &BoundaryMount, explicit: &[BoundaryMount]) -> bool {
    explicit.iter().any(|mirror| {
        alias
            .sandbox
            .strip_prefix(&mirror.sandbox)
            .is_ok_and(|relative| alias.host != mirror.host.join(relative))
    })
}

fn project(
    mirror: &BoundaryMount,
    path: &Path,
    access: PathAccess,
    optional: bool,
) -> Option<BoundaryMount> {
    if mirror.host.starts_with(path) {
        Some(BoundaryMount::alias(
            &mirror.host,
            &mirror.sandbox,
            access,
            optional,
        ))
    } else {
        path.strip_prefix(&mirror.host).ok().map(|relative| {
            BoundaryMount::alias(path, &mirror.sandbox.join(relative), access, optional)
        })
    }
}

fn append_region(
    mounts: &mut Vec<BoundaryMount>,
    mirrors: &[BoundaryMount],
    explicit: &[BoundaryMount],
    path: &Path,
    access: PathAccess,
    optional: bool,
) {
    mounts.push(BoundaryMount::direct(path, access, optional));
    if mirrors.is_empty() {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.into());
    // Intersect, never project an entire parent (or its unallowed siblings).
    let mut aliases: Vec<_> = mirrors
        .iter()
        .filter_map(|mirror| project(mirror, &canonical, access, optional))
        .filter(|alias| !shadowed_by_explicit(alias, explicit))
        .collect();
    sort_aliases(&mut aliases);

    // A broad cache projection can cover a differently sourced explicit mirror.
    // Restore that subtree after the projection, replaying the current host
    // precedence rather than resetting it to the declaration's base access.
    // This preserves prior thread/runtime RO children through later RW aliases.
    let mut restored = Vec::new();
    for mirror in explicit.iter().filter(|mirror| {
        aliases.iter().any(|alias| {
            mirror
                .sandbox
                .strip_prefix(&alias.sandbox)
                .is_ok_and(|relative| mirror.host != alias.host.join(relative))
        })
    }) {
        restored.push(mirror.clone());
        for region in mounts
            .iter()
            .filter(|m| m.host_region && (!m.cache || m.host.exists()))
        {
            let host = region
                .host
                .canonicalize()
                .unwrap_or_else(|_| region.host.clone());
            if let Some(alias) = project(mirror, &host, region.access, region.optional) {
                restored.push(alias);
            }
        }
    }
    mounts.extend(aliases);
    mounts.extend(restored);
}
