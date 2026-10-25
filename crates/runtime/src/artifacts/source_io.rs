//! Descriptor-relative source opens. Canonical names select an allowed root;
//! they are never opened again as absolute paths (including on non-Linux Unix).

use std::fs::File;
use std::path::{Component, Path};

use rustix::fs::{Mode, OFlags, open, openat};

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

pub(super) fn open_root(path: &Path) -> std::io::Result<File> {
    let mut dir = open("/", DIRECTORY, Mode::empty())?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => dir = openat(&dir, name, DIRECTORY, Mode::empty())?,
            _ => return Err(std::io::ErrorKind::InvalidInput.into()),
        }
    }
    Ok(dir.into())
}

pub(crate) fn open_directory(root: &File, relative: &Path) -> std::io::Result<File> {
    let mut directory = root.try_clone()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        directory = openat(&directory, name, DIRECTORY, Mode::empty())?.into();
    }
    Ok(directory)
}

/// Stage and rename relative to the pinned directory; a replaced symlink is
/// replaced itself, never followed. No absolute checked path is reopened.
pub(crate) fn store_image(directory: &File, name: &Path, bytes: &[u8]) -> Result<(), String> {
    use rustix::fs::{AtFlags, renameat, unlinkat};
    use std::io::Write;
    let temporary = format!(".{}.tmp", super::random_id("image")?);
    let result = (|| -> std::io::Result<()> {
        let mut file = File::from(openat(
            directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        file.write_all(bytes)?;
        file.sync_all()?;
        renameat(directory, temporary.as_str(), directory, name)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlinkat(directory, temporary.as_str(), AtFlags::empty());
    }
    result.map_err(|e| format!("cannot save generated image: {e}"))
}

pub(super) fn open_file(root: &File, relative: &Path) -> std::io::Result<File> {
    let mut dir = None;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(std::io::ErrorKind::InvalidInput.into());
        };
        let parent = dir.as_ref().unwrap_or(root);
        if components.peek().is_none() {
            // A FIFO swapped in at any point must not block open. The caller
            // checks the type with fstat on this very descriptor before reading.
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            return Ok(openat(parent, name, flags, Mode::empty())?.into());
        }
        dir = Some(File::from(openat(parent, name, DIRECTORY, Mode::empty())?));
    }
    Err(std::io::ErrorKind::InvalidInput.into())
}

#[cfg(test)]
type Hook = (std::path::PathBuf, Box<dyn FnOnce()>);

#[cfg(test)]
thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn before_open(path: &Path) {
    let hook = BEFORE_OPEN.with(|cell| {
        let mut pending = cell.borrow_mut();
        if pending.as_ref().is_some_and(|(target, _)| target == path) {
            pending.take()
        } else {
            None
        }
    });
    if let Some((_, hook)) = hook {
        hook();
    }
}

#[cfg(test)]
pub(super) fn with_before_open<T>(
    path: &Path,
    hook: impl FnOnce() + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BEFORE_OPEN.with(|cell| cell.borrow_mut().take());
        }
    }
    BEFORE_OPEN.with(|cell| {
        assert!(cell.borrow().is_none());
        cell.replace(Some((path.to_path_buf(), Box::new(hook))));
    });
    let _reset = Reset;
    run()
}
