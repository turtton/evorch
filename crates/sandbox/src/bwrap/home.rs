//! Prepare private mount points without following a sandbox-created symlink.

use crate::boundary::{BoundaryMount, MountPlan};
use crate::error::SandboxError;
use std::path::Path;

pub(super) fn prepare_home_aliases(plan: &MountPlan, scratch: &Path) -> Result<(), SandboxError> {
    let home = Path::new("/tmp/home");
    if !plan.mounts.iter().any(|m| m.home_alias) {
        return Ok(());
    }
    let invalid = |path: &Path, detail: String| SandboxError::InvalidSpec {
        detail: format!("HOME mirror {}: {detail}", path.display()),
    };
    for mount in &plan.mounts {
        if mount.host_region
            && !mount.cache
            && (home.starts_with(&mount.sandbox) || mount.sandbox.starts_with(home))
        {
            return Err(invalid(
                &mount.sandbox,
                "bind conflicts with private sandbox HOME".into(),
            ));
        }
    }
    let mut preceding: Vec<&BoundaryMount> = Vec::new();
    for mount in &plan.mounts {
        if (mount.cache || mount.optional) && !mount.host.exists() {
            continue;
        }
        if mount.home_alias {
            let directory = mount.host.is_dir();
            let mut path = home.to_path_buf();
            // Check each virtual ancestor against mounts already in effect,
            // including an earlier RO cache or a parent explicit HOME mirror.
            let components: Vec<_> = mount
                .sandbox
                .strip_prefix(home)
                .expect("HOME alias")
                .components()
                .collect();
            for index in 0..=components.len() {
                if index > 0 {
                    path.push(components[index - 1]);
                }
                let parent = preceding
                    .iter()
                    .rev()
                    .find(|m| path.starts_with(&m.sandbox))
                    .expect("/tmp scratch precedes HOME aliases");
                let relative = path.strip_prefix(&parent.sandbox).expect("covered path");
                check_mount_point(
                    &parent.host,
                    relative,
                    index < components.len() || directory,
                    parent.sandbox == Path::new("/tmp") && parent.host == scratch,
                )
                .map_err(|e| invalid(&path, e.to_string()))?;
            }
        }
        preceding.push(mount);
    }
    Ok(())
}

fn check_mount_point(
    root: &Path,
    relative: &Path,
    directory: bool,
    create: bool,
) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, mkdirat, open, openat, statat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if relative.as_os_str().is_empty() {
        let meta = root.symlink_metadata()?;
        return if meta.file_type().is_symlink() || meta.is_dir() != directory {
            Err(std::io::Error::other(
                "symlink or incompatible mount point type",
            ))
        } else {
            Ok(())
        };
    }
    let mut fd = open(root, flags, Mode::empty())?;
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(name) = part else {
            return Err(std::io::Error::other("non-normal HOME mirror path"));
        };
        if parts.peek().is_none() && !directory {
            return match statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                    Ok(())
                }
                Err(rustix::io::Errno::NOENT) if create => Ok(()), // bwrap creates the file.
                Ok(_) => Err(std::io::Error::other("incompatible file mount point")),
                Err(e) => Err(e.into()),
            };
        }
        fd = match openat(&fd, name, flags, Mode::empty()) {
            Err(rustix::io::Errno::NOENT) if create => {
                match mkdirat(&fd, name, Mode::RWXU) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
                openat(&fd, name, flags, Mode::empty())?
            }
            result => result?,
        };
    }
    Ok(())
}
