/// A private directory whose path stays short: owner hosts bind `<root>/<32 hex>.sock`
/// there, and a long `TMPDIR` would exceed `SUN_LEN`. It is a thin wrapper around
/// `test_socket_dir::SocketDir`, the one helper that picks the short base.
pub struct ShortPrivateDirectory(test_socket_dir::SocketDir);

impl ShortPrivateDirectory {
    pub fn new() -> Self {
        Self(test_socket_dir::SocketDir::new())
    }

    pub fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}
