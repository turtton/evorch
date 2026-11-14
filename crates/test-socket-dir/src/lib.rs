//! Short temporary directories for Unix domain sockets in tests.
//!
//! `bind(2)` rejects a socket path of `sun_path` bytes or more: 108 on Linux,
//! 104 on macOS. `tempfile::tempdir()` roots its directory at `$TMPDIR`, which
//! is long inside a sandbox (`/var/tmp/evorch-scratch-...`) and on macOS
//! (`/var/folders/...`), so a socket created below it can overflow the limit.
//! A [`SocketDir`] roots its directory at `$EVORCH_TEST_SOCKET_DIR`, or `/tmp`
//! when that is unset, and refuses to exist when the path leaves no room for
//! the socket file names a test creates in it.
//!
//! Keep databases, logs and other data in the regular `$TMPDIR`; only the
//! directory that holds sockets (for example an ownership root) belongs here.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Environment variable that overrides the base directory.
pub const BASE_DIR_ENV: &str = "EVORCH_TEST_SOCKET_DIR";

/// Base directory used when [`BASE_DIR_ENV`] is unset. `XDG_RUNTIME_DIR` is
/// deliberately not consulted: it is neither short nor present everywhere.
pub const DEFAULT_BASE_DIR: &str = "/tmp";

/// Bytes a socket path must stay below: `sizeof(sockaddr_un.sun_path)`,
/// which includes the terminating NUL.
pub const SUN_PATH_LIMIT: usize = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)) {
    104
} else {
    108
};

/// Bytes [`SocketDir::new`] reserves below its directory for sub-directories
/// and the socket file name. An ownership socket needs `/owners/` plus a
/// 32-digit id and `.sock`: 45 bytes.
pub const SUFFIX_BUDGET: usize = 64;

/// A 0700 directory, short enough to hold sockets, removed on drop.
#[derive(Debug)]
pub struct SocketDir {
    dir: tempfile::TempDir,
}

impl SocketDir {
    /// Creates a directory under `$EVORCH_TEST_SOCKET_DIR`, or `/tmp`.
    ///
    /// # Panics
    /// Panics when the directory cannot be created or its path is too long to
    /// hold a socket; see [`SocketDir::try_new_in`].
    #[track_caller]
    pub fn new() -> Self {
        let base = std::env::var_os(BASE_DIR_ENV).map_or_else(
            || PathBuf::from(DEFAULT_BASE_DIR),
            |value| {
                assert!(!value.is_empty(), "{BASE_DIR_ENV} is set but empty");
                PathBuf::from(value)
            },
        );
        Self::new_in(&base)
    }

    /// Like [`SocketDir::new`], with an explicit base directory.
    ///
    /// # Panics
    /// Panics on the errors of [`SocketDir::try_new_in`].
    #[track_caller]
    pub fn new_in(base: &Path) -> Self {
        Self::try_new_in(base).unwrap_or_else(|message| panic!("{message}"))
    }

    /// Creates a directory under `base` and checks that a socket fits in it.
    ///
    /// # Errors
    /// Returns a message naming the path, its length and the limit when the
    /// directory plus [`SUFFIX_BUDGET`] bytes would reach [`SUN_PATH_LIMIT`],
    /// and when the directory cannot be created. The directory is removed
    /// again in the first case.
    pub fn try_new_in(base: &Path) -> Result<Self, String> {
        let create_error = |error: std::io::Error| {
            format!(
                "cannot create a socket directory in {} (set {BASE_DIR_ENV} to a short, \
                 writable directory): {error}",
                base.display()
            )
        };
        let dir = tempfile::Builder::new()
            .prefix("es-")
            .tempdir_in(base)
            .map_err(create_error)?;
        // The mode follows the umask otherwise.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(create_error)?;
        let length = path_len(dir.path());
        if length + SUFFIX_BUDGET >= SUN_PATH_LIMIT {
            return Err(format!(
                "socket directory {} is {length} bytes; with {SUFFIX_BUDGET} bytes reserved for \
                 socket file names it must stay below the sun_path limit of {SUN_PATH_LIMIT} \
                 bytes. Set {BASE_DIR_ENV} to a shorter directory (default {DEFAULT_BASE_DIR}); \
                 $TMPDIR is not used because it is long in sandboxes and on macOS",
                dir.path().display(),
            ));
        }
        Ok(Self { dir })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The same directory as a plain `tempfile::TempDir`, for a fixture that
    /// stores one.
    pub fn into_tempdir(self) -> tempfile::TempDir {
        self.dir
    }

    /// `relative` below the directory, checked to fit in `sun_path`.
    ///
    /// # Panics
    /// Panics when the resulting path is `SUN_PATH_LIMIT` bytes or longer.
    #[track_caller]
    pub fn socket_path(&self, relative: impl AsRef<Path>) -> PathBuf {
        let path = self.dir.path().join(relative);
        let length = path_len(&path);
        assert!(
            length < SUN_PATH_LIMIT,
            "socket path {} is {length} bytes; the sun_path limit is {SUN_PATH_LIMIT} bytes",
            path.display()
        );
        path
    }
}

/// Shorthand for `SocketDir::new().into_tempdir()`: a drop-in for
/// `tempfile::tempdir().unwrap()` in a fixture whose directory holds sockets
/// (or an ownership root) next to its other files.
///
/// # Panics
/// Panics on the errors of [`SocketDir::new`].
#[track_caller]
pub fn tempdir() -> tempfile::TempDir {
    SocketDir::new().into_tempdir()
}

impl Default for SocketDir {
    #[track_caller]
    fn default() -> Self {
        Self::new()
    }
}

fn path_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_is_private_and_removed_on_drop() {
        let dir = SocketDir::new();
        let path = dir.path().to_path_buf();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let socket = dir.socket_path("owners/0123456789abcdef0123456789abcdef.sock");
        let listener_dir = socket.parent().unwrap();
        std::fs::create_dir_all(listener_dir).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn long_base_fails_with_an_explanation_and_leaves_nothing_behind() {
        let root = tempfile::tempdir().unwrap();
        let long = root.path().join("x".repeat(SUN_PATH_LIMIT));
        std::fs::create_dir(&long).unwrap();
        let message = SocketDir::try_new_in(&long).unwrap_err();
        assert!(message.contains(BASE_DIR_ENV), "{message}");
        assert!(message.contains(&SUN_PATH_LIMIT.to_string()), "{message}");
        assert_eq!(std::fs::read_dir(&long).unwrap().count(), 0);
    }

    #[test]
    #[should_panic(expected = "sun_path limit")]
    fn socket_path_rejects_an_overlong_name() {
        SocketDir::new().socket_path("n".repeat(SUN_PATH_LIMIT));
    }
}
