use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tools::mcp::{
    McpErrorKind,
    transport::{McpProcessLimiter, McpProcessPermit},
    types::MAX_STDIO_PER_PROJECT,
};
/// One service instance is shared by GUI tests, child runs and lazy restore.
#[derive(Default)]
pub struct ProjectMcpLimiter {
    projects: Mutex<BTreeMap<PathBuf, Weak<Semaphore>>>,
}
struct Permit {
    _permit: OwnedSemaphorePermit,
}
impl McpProcessPermit for Permit {}
impl McpProcessLimiter for ProjectMcpLimiter {
    fn try_acquire(&self, root: &Path) -> Result<Box<dyn McpProcessPermit>, McpErrorKind> {
        if !root.is_absolute() {
            return Err(McpErrorKind::Configuration);
        }
        let mut projects = self
            .projects
            .lock()
            .map_err(|_| McpErrorKind::Unavailable)?;
        projects.retain(|_, semaphore| semaphore.strong_count() > 0);
        let semaphore = projects
            .get(root)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let semaphore = Arc::new(Semaphore::new(MAX_STDIO_PER_PROJECT));
                projects.insert(root.to_path_buf(), Arc::downgrade(&semaphore));
                semaphore
            });
        let permit = semaphore
            .try_acquire_owned()
            .map_err(|_| McpErrorKind::ProcessLimitReached)?;
        Ok(Box::new(Permit { _permit: permit }))
    }
}
