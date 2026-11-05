/// A private directory whose path stays short: owner hosts bind `<root>/<32 hex>.sock`
/// there, and a long `TMPDIR` would exceed `SUN_LEN`. The path is relative to the test's
/// working directory (which this process never changes) and removed on drop.
pub struct ShortPrivateDirectory(std::path::PathBuf);

impl ShortPrivateDirectory {
    pub fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let name = uuid::Uuid::new_v4().simple().to_string();
        let path = std::path::PathBuf::from(format!(".o{}", &name[..8]));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ShortPrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
