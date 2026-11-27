//! Where the GUI and the core write the log that explains a lost connection.
//!
//! Both processes append to files in one directory, so a person who meets a
//! disconnect reads `core.log` and `gui.log` side by side. A file is capped: when
//! the next write would pass the cap it becomes `<name>.log.1` (replacing the
//! previous one) and a new file starts, so the pair never holds more than twice
//! the cap. Several processes may append to the same file; a process that is
//! still writing into a file another one has just rotated finishes into `.1`.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// The size at which a log file is rotated.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// `$XDG_STATE_HOME/evorch/logs`, else `$HOME/.local/state/evorch/logs`.
pub fn log_dir() -> Option<PathBuf> {
    log_dir_from(
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

pub fn log_dir_from(xdg_state: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    let state = xdg_state
        .filter(|dir| !dir.as_os_str().is_empty())
        .or_else(|| {
            home.filter(|dir| !dir.as_os_str().is_empty())
                .map(|home| home.join(".local/state"))
        })?;
    Some(state.join("evorch").join("logs"))
}

/// An append-only log file with a size cap. Owner-only, as the lines name paths
/// and endpoints.
pub struct RotatingLog {
    path: PathBuf,
    rotated: PathBuf,
    file: File,
    size: u64,
    max: u64,
}

impl RotatingLog {
    /// Open `<dir>/<name>.log` for appending, creating the directory.
    pub fn open(dir: &Path, name: &str, max: u64) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let path = dir.join(format!("{name}.log"));
        let rotated = dir.join(format!("{name}.log.1"));
        let (file, size) = open_append(&path)?;
        let mut log = Self {
            path,
            rotated,
            file,
            size,
            max,
        };
        if log.size >= log.max {
            log.rotate()?;
        }
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate(&mut self) -> io::Result<()> {
        fs::rename(&self.path, &self.rotated)?;
        let (file, size) = open_append(&self.path)?;
        self.file = file;
        self.size = size;
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<(File, u64)> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max {
            // Losing the rotation must not lose the line: keep appending.
            let _ = self.rotate();
        }
        let written = self.file.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_follows_xdg_state_then_home() {
        let dir = |xdg: Option<&str>, home: Option<&str>| {
            log_dir_from(xdg.map(PathBuf::from), home.map(PathBuf::from))
        };
        assert_eq!(
            dir(Some("/state"), Some("/home/u")),
            Some("/state/evorch/logs".into())
        );
        assert_eq!(
            dir(Some(""), Some("/home/u")),
            Some("/home/u/.local/state/evorch/logs".into())
        );
        assert_eq!(dir(None, None), None);
    }

    #[test]
    fn a_full_file_is_rotated_once_and_keeps_the_newest_lines() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("logs");
        let mut log = RotatingLog::open(&dir, "core", 16).unwrap();
        log.write_all(b"0123456789\n").unwrap();
        log.write_all(b"abcdefghij\n").unwrap();
        log.write_all(b"ABCDEFGHIJ\n").unwrap();
        // Each 11-byte line exceeds the 16-byte cap together with the previous
        // one: the file never holds two lines, and `.1` holds only the last
        // rotated one.
        assert_eq!(fs::read(dir.join("core.log")).unwrap(), b"ABCDEFGHIJ\n");
        assert_eq!(fs::read(dir.join("core.log.1")).unwrap(), b"abcdefghij\n");
        // A reopened oversized file is rotated before anything is appended.
        drop(log);
        let log = RotatingLog::open(&dir, "core", 8).unwrap();
        assert_eq!(fs::read(log.path()).unwrap(), b"");
        assert_eq!(fs::read(dir.join("core.log.1")).unwrap(), b"ABCDEFGHIJ\n");
    }
}
