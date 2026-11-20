use crate::admin::{self, AdminFailure};
use std::collections::BTreeMap;
use workbench_protocol::wire::Revision;

/// Bounded diagnostic cache only. Neither a CAS nor an owner/graph reservation.
#[derive(Default)]
pub(super) struct SnapshotClock {
    next: u64,
    observed: BTreeMap<String, (String, Revision)>,
}
impl SnapshotClock {
    pub fn observe(&mut self, key: &str, fingerprint: String) -> Result<Revision, AdminFailure> {
        if let Some((old, revision)) = self.observed.get(key)
            && old == &fingerprint
        {
            return Ok(*revision);
        }
        self.next = self.next.checked_add(1).ok_or_else(admin::unknown)?;
        if self.observed.len() >= 65 && !self.observed.contains_key(key) {
            self.observed.pop_first();
        }
        let revision = Revision(self.next);
        self.observed.insert(key.into(), (fingerprint, revision));
        Ok(revision)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_unchanged_but_no_revision_reuse_after_aba_or_eviction() {
        let mut c = SnapshotClock::default();
        let a = c.observe("a", "A".into()).unwrap();
        assert_eq!(a, c.observe("a", "A".into()).unwrap());
        assert!(c.observe("a", "B".into()).unwrap() > a);
        assert!(c.observe("a", "A".into()).unwrap() > a);
        for n in 0..66 {
            c.observe(&format!("graph-{n}"), n.to_string()).unwrap();
        }
        assert_eq!(c.observed.len(), 65);
        assert!(c.observe("a", "A".into()).unwrap() > a);
        c.next = u64::MAX;
        assert!(c.observe("new", "new".into()).is_err());
    }
}
