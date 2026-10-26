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

/// Staging never changes the destination. Drop cleans up even on cancellation.
pub(crate) struct StagedImage<'a> {
    directory: &'a File,
    temporary: String,
}

impl StagedImage<'_> {
    pub(crate) fn publish(&self, name: &Path) -> Result<(), String> {
        #[cfg(test)]
        PUBLISH_FAILURE.with(|count| {
            if let Some(remaining) = count.get() {
                count.set(remaining.checked_sub(1));
                if remaining == 1 {
                    return Err(format!(
                        "cannot publish generated image {}: injected renameat failure",
                        name.display()
                    ));
                }
            }
            Ok(())
        })?;
        rustix::fs::renameat(
            self.directory,
            self.temporary.as_str(),
            self.directory,
            name,
        )
        .map_err(|e| format!("cannot publish generated image {}: {e}", name.display()))
    }
}

impl Drop for StagedImage<'_> {
    fn drop(&mut self) {
        let _ = rustix::fs::unlinkat(
            self.directory,
            self.temporary.as_str(),
            rustix::fs::AtFlags::empty(),
        );
    }
}

pub(crate) fn validate_image_destination(directory: &File, name: &Path) -> Result<(), String> {
    use rustix::fs::{AtFlags, FileType, statat};
    match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => Ok(()),
        Err(rustix::io::Errno::NOENT) => Ok(()),
        _ => Err("output must be a regular file, not a symlink or directory".into()),
    }
}

pub(crate) fn stage_image<'a>(
    directory: &'a File,
    bytes: &[u8],
) -> Result<StagedImage<'a>, String> {
    use std::io::Write;
    let temporary = format!(".{}.tmp", super::random_id("image")?);
    let mut file = File::from(
        openat(
            directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|e| format!("cannot stage generated image: {e}"))?,
    );
    let staged = StagedImage {
        directory,
        temporary,
    };
    let result = (|| -> std::io::Result<()> {
        #[cfg(test)]
        STAGE_FAILURE.with(|count| {
            if let Some(remaining) = count.get() {
                count.set(remaining.checked_sub(1));
                if remaining == 1 {
                    return Err(std::io::Error::other("injected staging failure"));
                }
            }
            Ok(())
        })?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    result.map_err(|e| format!("cannot stage generated image: {e}"))?;
    Ok(staged)
}

#[cfg(test)]
thread_local! {
    static STAGE_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static PUBLISH_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_publish_failure<T>(nth: usize, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            PUBLISH_FAILURE.with(|count| count.set(None));
        }
    }
    PUBLISH_FAILURE.with(|count| {
        assert!(count.get().is_none());
        count.set(Some(nth));
    });
    let _reset = Reset;
    run()
}

#[cfg(test)]
pub(crate) fn with_stage_failure<T>(nth: usize, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            STAGE_FAILURE.with(|count| count.set(None));
        }
    }
    STAGE_FAILURE.with(|count| {
        assert!(count.get().is_none());
        count.set(Some(nth));
    });
    let _reset = Reset;
    run()
}

#[cfg(test)]
pub(crate) fn store_image(directory: &File, name: &Path, bytes: &[u8]) -> Result<(), String> {
    stage_image(directory, bytes)?.publish(name)
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
