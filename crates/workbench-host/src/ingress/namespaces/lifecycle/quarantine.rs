//! Independently registered quarantine leaf (Q1): bounded inspection and a
//! confirmed, revision-fenced discard of a cold quarantined graph's resume
//! eligibility. It never resumes, never acquires an owner and never rewrites
//! history; the common admin service owns audit, admission and authorization.
use crate::admin::AdminFacade;
use std::sync::{Arc, RwLock};

mod facade;
mod store;
#[cfg(test)]
mod tests;
mod view;

pub use facade::QuarantineFacade;
pub use store::StorageQuarantineStore;

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
