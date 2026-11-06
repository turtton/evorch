use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tools::mcp::{
    McpErrorKind,
    transport::{McpProcessLimiter, McpProcessPermit},
    types::{MAX_HTTP_PREPARATIONS, MAX_STDIO_PER_PROJECT},
};
/// One service instance is shared by GUI tests, child runs and lazy restore.
#[derive(Default)]
pub struct ProjectMcpLimiter {
    projects: Mutex<BTreeMap<PathBuf, Weak<Semaphore>>>,
    http: Mutex<BTreeMap<PathBuf, Weak<Semaphore>>>,
}
struct Permit {
    _permit: OwnedSemaphorePermit,
}
impl McpProcessPermit for Permit {}
impl ProjectMcpLimiter {
    pub(crate) fn try_http_preparation(
        &self,
        root: &Path,
    ) -> Result<OwnedSemaphorePermit, McpErrorKind> {
        acquire(
            &self.http,
            root,
            MAX_HTTP_PREPARATIONS,
            McpErrorKind::LimitExceeded,
        )
    }
}
impl McpProcessLimiter for ProjectMcpLimiter {
    fn try_acquire(&self, root: &Path) -> Result<Box<dyn McpProcessPermit>, McpErrorKind> {
        Ok(Box::new(Permit {
            _permit: acquire(
                &self.projects,
                root,
                MAX_STDIO_PER_PROJECT,
                McpErrorKind::ProcessLimitReached,
            )?,
        }))
    }
}
fn acquire(
    projects: &Mutex<BTreeMap<PathBuf, Weak<Semaphore>>>,
    root: &Path,
    limit: usize,
    full: McpErrorKind,
) -> Result<OwnedSemaphorePermit, McpErrorKind> {
    if !root.is_absolute() {
        return Err(McpErrorKind::Configuration);
    }
    let mut projects = projects.lock().map_err(|_| McpErrorKind::Unavailable)?;
    projects.retain(|_, semaphore| semaphore.strong_count() > 0);
    let semaphore = projects
        .get(root)
        .and_then(Weak::upgrade)
        .unwrap_or_else(|| {
            let semaphore = Arc::new(Semaphore::new(limit));
            projects.insert(root.to_path_buf(), Arc::downgrade(&semaphore));
            semaphore
        });
    semaphore.try_acquire_owned().map_err(|_| full)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn http_preparation_admission_is_project_shared_separate_and_released() {
        let limiter = ProjectMcpLimiter::default();
        let root = Path::new("/project");
        let stdio: Vec<_> = (0..MAX_STDIO_PER_PROJECT)
            .map(|_| limiter.try_acquire(root).unwrap())
            .collect();
        let http: Vec<_> = (0..MAX_HTTP_PREPARATIONS)
            .map(|_| limiter.try_http_preparation(root).unwrap())
            .collect();
        assert!(matches!(
            limiter.try_acquire(root),
            Err(McpErrorKind::ProcessLimitReached)
        ));
        assert!(matches!(
            limiter.try_http_preparation(root),
            Err(McpErrorKind::LimitExceeded)
        ));
        let other = limiter.try_http_preparation(Path::new("/other")).unwrap();
        drop(other);
        drop(http);
        assert!(limiter.try_http_preparation(root).is_ok());
        drop(stdio);
        assert!(limiter.try_acquire(root).is_ok());
    }
}
