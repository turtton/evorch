//! Independently registered owned_graphs leaf.
use crate::admin::AdminFacade;
use std::sync::{Arc, RwLock};
#[derive(Default)]
pub(crate) struct Registry(RwLock<Option<Arc<dyn AdminFacade>>>);
impl Registry {
    pub fn install(&self, f: Arc<dyn AdminFacade>) {
        *self.0.write().unwrap() = Some(f);
    }
    pub fn get(&self) -> Option<Arc<dyn AdminFacade>> {
        self.0.read().unwrap().clone()
    }
}
