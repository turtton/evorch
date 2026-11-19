//! Deterministic fake shared by O-H and integration tests. No Chromium.
use super::*;
use std::sync::Mutex;
type PageSender = watch::Sender<Option<Arc<ObservedPage>>>;
#[derive(Default)]
pub struct FakeObservationPort {
    pages: Mutex<Vec<(ObservationScope, PageSender)>>,
}
impl FakeObservationPort {
    pub fn publish(&self, page: ObservedPage) -> bool {
        if !page.validate() {
            return false;
        }
        let mut pages = self.pages.lock().unwrap();
        if let Some((_, tx)) = pages.iter_mut().find(|(s, _)| *s == page.snapshot.scope) {
            tx.send_replace(Some(Arc::new(page)));
        } else {
            if pages.len() >= MAX_RUNS {
                return false;
            }
            pages.push((
                page.snapshot.scope.clone(),
                watch::channel(Some(Arc::new(page))).0,
            ));
        }
        true
    }
    pub fn revoke(&self, scope: &ObservationScope) {
        self.pages.lock().unwrap().retain(|(s, _)| s != scope);
    }
}
impl AgentObservationPort for FakeObservationPort {
    fn list(
        &self,
        p: &ProjectId,
        t: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure> {
        Ok(self
            .pages
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| &s.project == p && &s.thread == t)
            .map(|(s, _)| s.clone())
            .collect())
    }
    fn snapshot(&self, s: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        self.pages
            .lock()
            .unwrap()
            .iter()
            .find(|(scope, _)| scope == s)
            .and_then(|(_, tx)| tx.borrow().clone())
            .ok_or(ObservationFailure::Revoked)
    }
    fn subscribe(&self, s: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
        self.pages
            .lock()
            .unwrap()
            .iter()
            .find(|(scope, _)| scope == s)
            .map(|(_, tx)| ObservationFeed {
                updates: tx.subscribe(),
            })
            .ok_or(ObservationFailure::Revoked)
    }
    fn current(&self, s: &ObservationScope) -> bool {
        self.snapshot(s).is_ok()
    }
}
