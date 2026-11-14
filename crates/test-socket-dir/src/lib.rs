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
//! [`SplitDir`] bundles both for a fixture that passes one directory around.

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
        check_directory_length(dir.path())?;
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

/// A fixture directory split in two: [`SplitDir::path`] is a regular
/// `tempfile` directory below `$TMPDIR` for databases, logs and state, and
/// [`SplitDir::sockets`] is a short [`SocketDir`] for sockets and ownership
/// roots. Both are removed on drop.
///
/// Use it where a fixture hands one directory around but a socket must still
/// fit in `sun_path`: data stays out of `/tmp` (often a tmpfs), only the socket
/// directory lives there.
#[derive(Debug)]
pub struct SplitDir {
    data: tempfile::TempDir,
    sockets: SocketDir,
}

impl SplitDir {
    /// Creates both directories.
    ///
    /// # Panics
    /// Panics when either cannot be created or the socket directory is too
    /// long; see [`SocketDir::new`].
    #[track_caller]
    pub fn new() -> Self {
        Self {
            data: tempfile::tempdir().expect("create the data directory below $TMPDIR"),
            sockets: SocketDir::new(),
        }
    }

    /// The data directory below `$TMPDIR`.
    pub fn path(&self) -> &Path {
        self.data.path()
    }

    /// The short directory for sockets and ownership roots.
    pub fn sockets(&self) -> &Path {
        self.sockets.path()
    }
}

impl Default for SplitDir {
    #[track_caller]
    fn default() -> Self {
        Self::new()
    }
}

impl Default for SocketDir {
    #[track_caller]
    fn default() -> Self {
        Self::new()
    }
}

/// Fails when a socket file name of [`SUFFIX_BUDGET`] bytes below `directory` would reach
/// [`SUN_PATH_LIMIT`].
fn check_directory_length(directory: &Path) -> Result<(), String> {
    let length = path_len(directory);
    if length + SUFFIX_BUDGET >= SUN_PATH_LIMIT {
        return Err(format!(
            "socket directory {} is {length} bytes; with {SUFFIX_BUDGET} bytes reserved for \
             socket file names it must stay below the sun_path limit of {SUN_PATH_LIMIT} \
             bytes. Set {BASE_DIR_ENV} to a shorter directory (default {DEFAULT_BASE_DIR}); \
             $TMPDIR is not used because it is long in sandboxes and on macOS",
            directory.display(),
        ));
    }
    Ok(())
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
    fn split_dir_keeps_data_in_tmpdir_and_sockets_in_the_short_base() {
        let dir = SplitDir::new();
        let sockets = dir.sockets().to_path_buf();
        let data = dir.path().to_path_buf();
        assert!(sockets.starts_with(
            std::env::var_os(BASE_DIR_ENV).unwrap_or_else(|| DEFAULT_BASE_DIR.into())
        ));
        assert!(data.starts_with(std::env::temp_dir()));
        assert_ne!(data, sockets);
        drop(dir);
        assert!(!sockets.exists() && !data.exists());
    }

    #[test]
    fn directories_created_at_the_same_time_never_share_a_name() {
        const THREADS: usize = 16;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    SocketDir::new()
                })
            })
            .collect();
        let dirs: Vec<SocketDir> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let names: std::collections::HashSet<_> =
            dirs.iter().map(|dir| dir.path().to_path_buf()).collect();
        assert_eq!(names.len(), THREADS);
    }

    #[test]
    fn drop_removes_a_symlink_but_never_what_it_points_to() {
        let outside = tempfile::tempdir().unwrap();
        let keep = outside.path().join("keep.txt");
        std::fs::write(&keep, "outside").unwrap();
        let dir = SocketDir::new();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink(&keep, dir.path().join("file-link")).unwrap();
        let path = dir.path().to_path_buf();
        drop(dir);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "outside");
    }

    #[test]
    fn the_length_check_is_exclusive_at_the_reserved_budget() {
        let of_len = |length: usize| PathBuf::from(format!("/{}", "d".repeat(length - 1)));
        let largest_ok = SUN_PATH_LIMIT - SUFFIX_BUDGET - 1;
        assert!(check_directory_length(&of_len(largest_ok)).is_ok());
        let message = check_directory_length(&of_len(largest_ok + 1)).unwrap_err();
        assert!(
            message.contains(&format!("is {} bytes", largest_ok + 1)),
            "{message}"
        );
        assert!(message.contains(BASE_DIR_ENV), "{message}");
    }

    /// Child half of [`the_base_directory_follows_the_environment_in_a_child_process`]; it
    /// reports where [`SocketDir::new`] put its directory under the environment it was given.
    #[test]
    #[ignore = "subprocess entry point; run by the parent test"]
    fn child_reports_its_socket_directory() {
        println!("SOCKET_DIR={}", SocketDir::new().path().display());
    }

    #[test]
    fn the_base_directory_follows_the_environment_in_a_child_process() {
        let run = |base: Option<&Path>| -> PathBuf {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "tests::child_reports_its_socket_directory",
                "--ignored",
                "--nocapture",
            ]);
            match base {
                Some(base) => command.env(BASE_DIR_ENV, base),
                None => command.env_remove(BASE_DIR_ENV),
            };
            // The default base must not come from these.
            command
                .env("XDG_RUNTIME_DIR", "/nonexistent")
                .env("TMPDIR", "/nonexistent");
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(
                output.status.success(),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let line = stdout
                .lines()
                .find_map(|l| l.strip_prefix("SOCKET_DIR="))
                .unwrap();
            PathBuf::from(line)
        };
        let chosen = SocketDir::new();
        assert!(run(Some(chosen.path())).starts_with(chosen.path()));
        assert!(run(None).starts_with(DEFAULT_BASE_DIR));
    }

    #[test]
    #[should_panic(expected = "sun_path limit")]
    fn socket_path_rejects_an_overlong_name() {
        SocketDir::new().socket_path("n".repeat(SUN_PATH_LIMIT));
    }
}
