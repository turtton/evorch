//! Archive and pin state of a conversation family.
//!
//! A family is every conversation that descends, by `ThreadRecord::tree_parent`, from one
//! top ancestor: forks, rewound versions, escalated conversations, lanes a lead
//! manages and their descendants (a detached lane starts a family of its own). Its key is that top ancestor's ID, which does not depend on the
//! version shown or on the thread in view. The family owns **one** archive series and
//! **one** pin series ([`FamilyState`]); a thread record holds no state of its own.
//!
//! * Archiving, restoring, pinning and unpinning are the only edits of a state, and
//!   each numbers its own series (done under the sidebar save lock by the host).
//! * Adding a conversation to a family (fork, rewind, escalation, replay) or showing
//!   another version changes no state and numbers nothing.
//! * `archived`, `archived_at` and `pinned` on [`ThreadRecord`] are a derived cache for
//!   the views that read a record; [`SidebarState::refresh_derived`] is their only
//!   writer. Anything that must agree with the sidebar (the database projection, the
//!   retention input, the display) takes [`SidebarState::resolve_states`] instead.
//! * A thread whose family cannot be found (missing parent, cycle) is *unresolved*:
//!   it has no state, it is never deleted and nothing is projected for it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{SidebarState, ThreadError, ThreadId};

/// The newest sidebar version: families own the archive and pin state.
pub(crate) const FAMILY_SCHEMA_VERSION: u32 = 2;

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The archive and pin series of one family. A family without a state is not
/// archived and not pinned, at revision 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyState {
    /// The family's top ancestor.
    pub key: ThreadId,
    /// Independent membership period. Revisions below are comparable only within it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub membership_epoch: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// UNIX seconds the family was archived at; `None` when it is not archived or was
    /// archived before the time was recorded (such an archive is due at once).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    /// Counts the edits of `archived` / `archived_at`. Merging two sidebars keeps the
    /// series with the higher revision, so a stale snapshot never undoes a newer edit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub archive_revision: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Counts the edits of `pinned`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pin_revision: u64,
}

impl FamilyState {
    fn new(key: ThreadId) -> Self {
        Self {
            key,
            membership_epoch: 0,
            archived: false,
            archived_at: None,
            archive_revision: 0,
            pinned: false,
            pin_revision: 0,
        }
    }
}

/// Why a thread belongs to no family that can be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyUnresolved {
    /// `thread` names a parent that is not in the sidebar.
    MissingParent { thread: ThreadId, parent: ThreadId },
    /// The parent chain of `thread` leads back to itself.
    Cycle { thread: ThreadId },
    /// Two sidebars gave `thread` different parents.
    ParentConflict { thread: ThreadId },
}

impl std::fmt::Display for FamilyUnresolved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingParent { thread, parent } => {
                write!(formatter, "thread {thread} names a missing parent {parent}")
            }
            Self::Cycle { thread } => {
                write!(formatter, "the parents of thread {thread} form a cycle")
            }
            Self::ParentConflict { thread } => {
                write!(formatter, "thread {thread} was given different parents")
            }
        }
    }
}

/// What one thread's family says about it: the single resolution every view uses
/// (display, database projection, retention input and schedule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedState {
    /// The family key, or why there is none.
    pub family: Result<ThreadId, FamilyUnresolved>,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub archive_revision: u64,
    pub pinned: bool,
    pub pin_revision: u64,
}

impl ResolvedState {
    fn unresolved(reason: FamilyUnresolved) -> Self {
        Self {
            family: Err(reason),
            archived: false,
            archived_at: None,
            archive_revision: 0,
            pinned: false,
            pin_revision: 0,
        }
    }

    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        self.family.is_ok()
    }
}

/// What a one-time conversion of an older sidebar did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Families found.
    pub families: usize,
    /// Families that came out archived.
    pub archived: usize,
    /// Families that came out pinned.
    pub pinned: usize,
    /// Families whose members disagreed about being archived: the whole family is
    /// treated as not archived (the side that deletes nothing), with no new time.
    pub mixed: usize,
    /// Threads whose family could not be found; they keep no archive or pin state.
    pub unresolved: usize,
}

impl SidebarState {
    /// Every thread's family key, or why it has none. Parent chains are followed once
    /// per thread and memoised.
    fn family_keys(&self) -> BTreeMap<ThreadId, Result<ThreadId, FamilyUnresolved>> {
        let parents: BTreeMap<&ThreadId, Option<&ThreadId>> = self
            .threads
            .iter()
            .map(|thread| (&thread.id, thread.tree_parent()))
            .collect();
        let mut keys: BTreeMap<ThreadId, Result<ThreadId, FamilyUnresolved>> = BTreeMap::new();
        for thread in &self.threads {
            if keys.contains_key(&thread.id) {
                continue;
            }
            let mut chain: Vec<&ThreadId> = Vec::new();
            let mut seen: BTreeSet<&ThreadId> = BTreeSet::new();
            let mut current = &thread.id;
            let outcome = loop {
                if let Some(known) = keys.get(current) {
                    break known.clone();
                }
                if !seen.insert(current) {
                    break Err(FamilyUnresolved::Cycle {
                        thread: thread.id.clone(),
                    });
                }
                chain.push(current);
                if self.parent_conflicts.contains(current) {
                    break Err(FamilyUnresolved::ParentConflict {
                        thread: current.clone(),
                    });
                }
                match parents.get(current) {
                    Some(Some(parent)) if parents.contains_key(parent) => current = parent,
                    Some(Some(parent)) => {
                        break Err(FamilyUnresolved::MissingParent {
                            thread: current.clone(),
                            parent: (*parent).clone(),
                        });
                    }
                    _ => break Ok(current.clone()),
                }
            };
            for id in chain {
                keys.insert(id.clone(), outcome.clone());
            }
        }
        keys
    }

    /// The state of every thread. The one resolution function: the display cache, the
    /// database projection, the retention input and the schedule all read this.
    #[must_use]
    pub fn resolve_states(&self) -> BTreeMap<ThreadId, ResolvedState> {
        let states: BTreeMap<&ThreadId, &FamilyState> = self
            .families
            .iter()
            .map(|state| (&state.key, state))
            .collect();
        let epochs: BTreeMap<_, _> = self
            .threads
            .iter()
            .filter_map(|thread| {
                let lane = thread.lane.as_ref()?;
                (!lane.policy.is_managed()).then_some((
                    &thread.id,
                    lane.detached_since_management_revision.unwrap_or(0),
                ))
            })
            .collect();
        self.family_keys()
            .into_iter()
            .map(|(id, family)| {
                let resolved = match family {
                    Err(reason) => ResolvedState::unresolved(reason),
                    Ok(key) => {
                        let state = states.get(&key).copied().cloned();
                        let epoch = epochs.get(&key).copied().unwrap_or(0);
                        let state = state
                            .filter(|state| state.membership_epoch == epoch)
                            .unwrap_or_else(|| {
                                let mut state = FamilyState::new(key.clone());
                                state.membership_epoch = epoch;
                                state
                            });
                        ResolvedState {
                            family: Ok(key),
                            archived: state.archived,
                            archived_at: state.archived_at,
                            archive_revision: state.archive_revision,
                            pinned: state.pinned,
                            pin_revision: state.pin_revision,
                        }
                    }
                };
                (id, resolved)
            })
            .collect()
    }

    /// The family `id` belongs to.
    ///
    /// # Errors
    /// `id` is unknown, or its family cannot be found.
    pub fn family_key(&self, id: &ThreadId) -> Result<ThreadId, ThreadError> {
        match self.family_keys().remove(id) {
            None => Err(ThreadError::UnknownThread),
            Some(Err(reason)) => Err(ThreadError::UnresolvedFamily(reason.to_string())),
            Some(Ok(key)) => Ok(key),
        }
    }

    /// The members of the family `key` (the key included), in list order.
    #[must_use]
    pub fn family_members(&self, key: &ThreadId) -> Vec<ThreadId> {
        let keys = self.family_keys();
        self.threads
            .iter()
            .filter(|thread| keys.get(&thread.id).and_then(|k| k.as_ref().ok()) == Some(key))
            .map(|thread| thread.id.clone())
            .collect()
    }

    /// The threads whose family cannot be found.
    #[must_use]
    pub fn unresolved_threads(&self) -> Vec<(ThreadId, FamilyUnresolved)> {
        self.family_keys()
            .into_iter()
            .filter_map(|(id, family)| family.err().map(|reason| (id, reason)))
            .collect()
    }

    /// Recomputes the derived `archived`, `archived_at` and `pinned` of every record
    /// (and clears the legacy revisions of an older file). The only writer of them.
    pub fn refresh_derived(&mut self) {
        let independent: Vec<_> = self
            .threads
            .iter()
            .filter_map(|thread| {
                let lane = thread.lane.as_ref()?;
                (!lane.policy.is_managed() && lane.detached_since_management_revision.is_some())
                    .then(|| thread.id.clone())
            })
            .collect();
        for key in independent {
            self.family_entry(&key);
        }
        let states = self.resolve_states();
        for thread in &mut self.threads {
            let state = states.get(&thread.id);
            thread.archived = state.is_some_and(|state| state.archived);
            thread.archived_at = state.and_then(|state| state.archived_at);
            thread.pinned = state.is_some_and(|state| state.pinned);
            thread.archive_revision = 0;
            thread.pin_revision = 0;
        }
    }

    pub(crate) fn membership_epoch(&self, key: &ThreadId) -> u64 {
        self.threads
            .iter()
            .find(|thread| &thread.id == key)
            .and_then(|thread| thread.lane.as_ref())
            .filter(|lane| !lane.policy.is_managed())
            .and_then(|lane| lane.detached_since_management_revision)
            .unwrap_or(0)
    }

    fn family_entry(&mut self, key: &ThreadId) -> &mut FamilyState {
        let epoch = self.membership_epoch(key);
        if let Some(position) = self.families.iter().position(|state| &state.key == key) {
            if self.families[position].membership_epoch != epoch {
                self.families[position] = FamilyState::new(key.clone());
                self.families[position].membership_epoch = epoch;
            }
            return &mut self.families[position];
        }
        let mut state = FamilyState::new(key.clone());
        state.membership_epoch = epoch;
        self.families.push(state);
        let last = self.families.len() - 1;
        &mut self.families[last]
    }

    /// Archives or restores the family of `id`, numbering the edit. Restoring clears
    /// the time. A call that changes nothing leaves the revision as it is. Returns
    /// whether anything changed.
    ///
    /// # Errors
    /// `id` is unknown or its family cannot be found.
    pub fn set_family_archived(
        &mut self,
        id: &ThreadId,
        archived: bool,
        at: i64,
    ) -> Result<bool, ThreadError> {
        let key = self.family_key(id)?;
        let state = self.family_entry(&key);
        if state.archived == archived && (!archived || state.archived_at.is_some()) {
            return Ok(false);
        }
        state.archived = archived;
        state.archived_at = archived.then_some(at);
        state.archive_revision += 1;
        self.refresh_derived();
        Ok(true)
    }

    /// Pins or unpins the family of `id`, numbering the edit. Returns whether
    /// anything changed.
    ///
    /// # Errors
    /// `id` is unknown or its family cannot be found.
    pub fn set_family_pinned(&mut self, id: &ThreadId, pinned: bool) -> Result<bool, ThreadError> {
        let key = self.family_key(id)?;
        let state = self.family_entry(&key);
        if state.pinned == pinned {
            return Ok(false);
        }
        state.pinned = pinned;
        state.pin_revision += 1;
        self.refresh_derived();
        Ok(true)
    }

    /// Raises the series of every family above the revisions an external projection
    /// already holds (`floor` gives a thread's archive and pin revision there), so
    /// what the sidebar says is never ignored for being older. A family without a
    /// state gets one that says what it said before. Returns how many were raised.
    ///
    /// A projection at the *same* revision keeps its own value when the two disagree
    /// (the protective side wins there), so the sidebar's value could never replace it.
    /// `converted` is for a sidebar just made from an older file, whose numbers were
    /// invented without knowing the projection's: it must end strictly above it, equal
    /// numbers included. Otherwise only a projection strictly ahead is passed.
    pub fn raise_family_revisions(
        &mut self,
        floor: &dyn Fn(&ThreadId) -> (u64, u64),
        converted: bool,
    ) -> usize {
        let keys = self.family_keys();
        let mut highest: BTreeMap<ThreadId, (u64, u64)> = BTreeMap::new();
        for thread in &self.threads {
            if let Some(Ok(key)) = keys.get(&thread.id) {
                let (archive, pin) = floor(&thread.id);
                let entry = highest.entry(key.clone()).or_default();
                entry.0 = entry.0.max(archive);
                entry.1 = entry.1.max(pin);
            }
        }
        let needs = |floor: u64, current: u64| {
            if converted {
                floor > 0 && floor >= current
            } else {
                floor > current
            }
        };
        let mut raised = 0;
        for (key, (archive, pin)) in highest {
            let current = self
                .families
                .iter()
                .find(|state| state.key == key)
                .map_or((0, 0), |state| (state.archive_revision, state.pin_revision));
            if !needs(archive, current.0) && !needs(pin, current.1) {
                continue;
            }
            let state = self.family_entry(&key);
            if needs(archive, current.0) {
                state.archive_revision = archive + 1;
            }
            if needs(pin, current.1) {
                state.pin_revision = pin + 1;
            }
            raised += 1;
        }
        raised
    }

    /// Makes a sidebar ready to use after it was read: converts an older file
    /// (once), then derives the per-record cache. Returns the conversion's report
    /// when one ran.
    pub fn settle_loaded(&mut self) -> Option<MigrationReport> {
        let report = (self.version < FAMILY_SCHEMA_VERSION).then(|| self.migrate_legacy());
        self.refresh_derived();
        report
    }

    /// The explicit, one-time conversion of an older sidebar, whose threads each kept
    /// an archive and a pin of their own, into family states.
    ///
    /// * Members all archived: the family is archived, dated by the latest time any
    ///   member recorded (none when none did: a legacy archive stays due at once).
    /// * Members disagreeing: the whole family is not archived and gets no time.
    /// * Any member pinned: the family is pinned (the side that deletes nothing).
    /// * Each series starts above every revision the members carried.
    fn migrate_legacy(&mut self) -> MigrationReport {
        let keys = self.family_keys();
        let mut report = MigrationReport::default();
        let mut groups: BTreeMap<ThreadId, Vec<usize>> = BTreeMap::new();
        for (index, thread) in self.threads.iter().enumerate() {
            match keys.get(&thread.id) {
                Some(Ok(key)) => groups.entry(key.clone()).or_default().push(index),
                _ => report.unresolved += 1,
            }
        }
        let mut states: Vec<FamilyState> = Vec::new();
        for (key, members) in groups {
            report.families += 1;
            let records: Vec<_> = members.iter().map(|&index| &self.threads[index]).collect();
            let archived_members = records.iter().filter(|thread| thread.archived).count();
            let archived = archived_members == records.len();
            if archived_members > 0 && !archived {
                report.mixed += 1;
            }
            let pinned = records.iter().any(|thread| thread.pinned);
            let archive_revision = records
                .iter()
                .map(|thread| thread.archive_revision)
                .max()
                .unwrap_or(0);
            let pin_revision = records
                .iter()
                .map(|thread| thread.pin_revision)
                .max()
                .unwrap_or(0);
            let touched =
                archived_members > 0 || pinned || archive_revision > 0 || pin_revision > 0;
            if !touched {
                continue;
            }
            report.archived += usize::from(archived);
            report.pinned += usize::from(pinned);
            states.push(FamilyState {
                membership_epoch: self.membership_epoch(&key),
                archived_at: archived
                    .then(|| records.iter().filter_map(|thread| thread.archived_at).max())
                    .flatten(),
                archived,
                archive_revision: archive_revision + 1,
                pinned,
                pin_revision: pin_revision + 1,
                key,
            });
        }
        self.families = states;
        self.version = FAMILY_SCHEMA_VERSION;
        report
    }
}

/// The merge of two sidebars' family states: by key, each series keeps the side with
/// the higher revision (so no edit of a stale snapshot undoes a newer one and no
/// revision goes back); at equal revisions with different content the protective side
/// (archived, pinned) wins. A state only one side has is kept as it is, so two windows
/// that both create the state of one family end with one state.
pub(crate) fn merge_families(local: &[FamilyState], disk: &[FamilyState]) -> Vec<FamilyState> {
    let mut merged: Vec<FamilyState> = Vec::with_capacity(local.len().max(disk.len()));
    for state in local {
        match disk.iter().find(|other| other.key == state.key) {
            Some(other) => merged.push(merge_family(state, other)),
            None => merged.push(state.clone()),
        }
    }
    for state in disk {
        if !local.iter().any(|other| other.key == state.key) {
            merged.push(state.clone());
        }
    }
    merged
}

fn merge_family(local: &FamilyState, disk: &FamilyState) -> FamilyState {
    match local.membership_epoch.cmp(&disk.membership_epoch) {
        std::cmp::Ordering::Greater => return local.clone(),
        std::cmp::Ordering::Less => return disk.clone(),
        std::cmp::Ordering::Equal => {}
    }
    // A higher revision replaces the series. At an equal revision the sides were made
    // independently from the same base; the result must not depend on which side is
    // "local", and keeps the protective side: archived wins, and of two archive times
    // the later one (the longer retention) wins.
    let (archived, archived_at, archive_revision) = match local
        .archive_revision
        .cmp(&disk.archive_revision)
    {
        std::cmp::Ordering::Greater => (local.archived, local.archived_at, local.archive_revision),
        std::cmp::Ordering::Less => (disk.archived, disk.archived_at, disk.archive_revision),
        std::cmp::Ordering::Equal => {
            let archived = local.archived || disk.archived;
            let at = [local, disk]
                .into_iter()
                .filter(|side| side.archived)
                .filter_map(|side| side.archived_at)
                .max();
            (archived, at, local.archive_revision)
        }
    };
    let (pinned, pin_revision) = match local.pin_revision.cmp(&disk.pin_revision) {
        std::cmp::Ordering::Greater => (local.pinned, local.pin_revision),
        std::cmp::Ordering::Less => (disk.pinned, disk.pin_revision),
        std::cmp::Ordering::Equal => (local.pinned || disk.pinned, local.pin_revision),
    };
    FamilyState {
        key: local.key.clone(),
        membership_epoch: local.membership_epoch,
        archived,
        archived_at,
        archive_revision,
        pinned,
        pin_revision,
    }
}
