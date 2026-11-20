//! Independently registered quarantine leaf (Q1): bounded inspection and a
//! confirmed, revision-fenced discard of a cold quarantined graph's resume
//! eligibility. It never resumes, never acquires an owner and never rewrites
//! history; the common admin service owns audit, admission and authorization.
use crate::admin::AdminFacade;
use std::sync::{Arc, RwLock};

// The 5a-V bootstrap constructs and installs the facade; until then nothing
// outside this leaf names it. That integration PR removes these allows.
#[allow(dead_code, reason = "installed by the 5a-V bootstrap")]
mod facade;
#[allow(dead_code, reason = "installed by the 5a-V bootstrap")]
mod store;
#[cfg(test)]
mod tests;
#[allow(dead_code, reason = "installed by the 5a-V bootstrap")]
mod view;

#[allow(unused_imports, reason = "installed by the 5a-V bootstrap")]
pub use facade::QuarantineFacade;
#[allow(unused_imports, reason = "installed by the 5a-V bootstrap")]
pub use store::{QuarantineStore, StorageQuarantineStore};

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
