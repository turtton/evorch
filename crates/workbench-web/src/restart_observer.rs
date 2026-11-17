//! Injectable observation only: no new dependency or production configuration.
#[cfg(any(test, feature = "test-support"))]
use std::sync::{Arc, Mutex};
#[cfg(any(test, feature = "test-support"))]
type Callback = Arc<dyn Fn(&'static str) + Send + Sync>;

#[derive(Clone, Default)]
pub(crate) struct Observer {
    #[cfg(any(test, feature = "test-support"))]
    callback: Arc<Mutex<Option<Callback>>>,
}
impl Observer {
    #[inline]
    pub(crate) fn record(&self, _phase: &'static str) {
        #[cfg(any(test, feature = "test-support"))]
        {
            let callback = self.callback.lock().unwrap().clone();
            if let Some(callback) = callback {
                callback(_phase);
            }
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set(&self, callback: Callback) {
        *self.callback.lock().unwrap() = Some(callback);
    }
}
