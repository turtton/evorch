//! Test-only per-goal barriers and failure injection; absent from product builds.
use crate::StorageError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    Read,
    BeforeCommit,
    AfterAck,
    AfterFailure,
}
type Hook = Arc<dyn Fn(Point) -> Result<(), StorageError> + Send + Sync>;
fn hooks() -> &'static Mutex<HashMap<String, Hook>> {
    static HOOKS: OnceLock<Mutex<HashMap<String, Hook>>> = OnceLock::new();
    HOOKS.get_or_init(Mutex::default)
}
pub struct Installed(String);
impl Drop for Installed {
    fn drop(&mut self) {
        hooks().lock().unwrap().remove(&self.0);
    }
}
pub fn install(
    id: &str,
    hook: impl Fn(Point) -> Result<(), StorageError> + Send + Sync + 'static,
) -> Installed {
    assert!(
        hooks()
            .lock()
            .unwrap()
            .insert(id.into(), Arc::new(hook))
            .is_none()
    );
    Installed(id.into())
}
pub(crate) fn visit(id: &str, point: Point) -> Result<(), StorageError> {
    let hook = hooks().lock().unwrap().get(id).cloned();
    hook.map_or(Ok(()), |hook| hook(point))
}
