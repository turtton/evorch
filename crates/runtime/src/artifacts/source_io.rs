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
