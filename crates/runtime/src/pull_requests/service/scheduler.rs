//! The loop that decides when each tracked pull request is fetched.
//!
//! It plans from the database and the ownership registry, never from a copy: the links of
//! unarchived threads, the ones this process owns, and the pending inheritances of owned
//! children. It sleeps until the earliest due time or until something it plans from changes
//! (a link, an unlink, an archive, a settings change, an ownership change, a refresh, the end
//! of a command that publishes), and wakes through [`Notify`](tokio::sync::Notify) and watch
//! channels, never by polling at a fixed rate.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use event_bus::PullRequestAutoFixChange;
use storage::StorageError;
use storage::pull_requests::{InheritanceResolution, PullRequestKey, PullRequestState};
use tokio::time::Instant;

use super::flight::{Flight, Interest, Target};
use super::{Inner, KeyState, Stage};
use crate::ownership::OwnerPermit;

#[cfg(test)]
mod tests;

/// Rows read per page of a listing.
const PAGE: usize = 200;

/// Most pending inheritances looked at in one round.
const MAX_CHILDREN_PER_ROUND: usize = 1_000;

struct Plan {
    targets: Vec<Target>,
    /// For each key with stored facts: their `success_seq`, and whether they say merged or closed.
    stored: HashMap<PullRequestKey, (u64, bool)>,
    /// Pending inheritances of children this process owns: the child, the pull request it waits
    /// for, and its permit.
    children: Vec<(String, PullRequestKey, OwnerPermit)>,
}

pub(super) async fn run(inner: Arc<Inner>) {
    let mut settings = inner.settings.subscribe();
    let mut shutdown = inner.shutdown.subscribe();
    let mut applied = *settings.borrow();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let config = *settings.borrow_and_update();
        // Whatever woke the loop, new settings are applied once: judged by the value, not by
        // which branch of the select won.
        if config != applied {
            inner.apply_new_settings();
            if config.auto_fix_enabled != applied.auto_fix_enabled {
                inner.announce_global_auto_fix().await;
            }
            applied = config;
        }
        let due = if config.enabled {
            inner.round().await
        } else {
            None
        };
        tokio::select! {
            () = inner.wake.notified() => {}
            changed = settings.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            () = sleep_until(due) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

fn earlier(current: Option<Instant>, candidate: Instant) -> Option<Instant> {
    Some(current.map_or(candidate, |current| current.min(candidate)))
}

fn owned(
    inner: &Inner,
    cache: &mut HashMap<String, Option<OwnerPermit>>,
    thread_id: &str,
) -> Option<OwnerPermit> {
    cache
        .entry(thread_id.to_owned())
        .or_insert_with(|| inner.ownership.owned_permit(thread_id))
        .clone()
}

fn plan_blocking(inner: &Inner) -> Result<Plan, StorageError> {
    let mut cache = HashMap::new();
    let mut targets = Vec::new();
    let mut after = 0;
    loop {
        let page = inner.storage.active_pull_request_links(after, PAGE)?;
        let full = page.len() == PAGE;
        for link in page {
            after = link.link_id;
            if let Some(permit) = owned(inner, &mut cache, &link.thread_id) {
                targets.push(Target {
                    thread_id: link.thread_id,
                    key: link.key,
                    link_id: link.link_id,
                    link_revision: link.link_revision,
                    permit,
                });
            }
        }
        if !full {
            break;
        }
    }
    let mut stored = HashMap::new();
    for target in &targets {
        if stored.contains_key(&target.key) {
            continue;
        }
        if let Some(snapshot) = inner.storage.pull_request_snapshot(&target.key)?
            && let Some(facts) = snapshot.facts
        {
            stored.insert(
                target.key.clone(),
                (snapshot.success_seq, facts.state != PullRequestState::Open),
            );
        }
    }
    let mut children = Vec::new();
    let mut cursor = None;
    loop {
        let page = inner
            .storage
            .pending_pull_request_inheritances(cursor.take(), PAGE)?;
        let full = page.len() == PAGE;
        for record in &page {
            if let Some(permit) = owned(inner, &mut cache, &record.child_thread_id) {
                children.push((record.child_thread_id.clone(), record.key.clone(), permit));
            }
        }
        if !full || children.len() >= MAX_CHILDREN_PER_ROUND {
            break;
        }
        cursor = page
            .last()
            .map(storage::pull_requests::InheritanceRecord::cursor);
    }
    Ok(Plan {
        targets,
        stored,
        children,
    })
}

impl Inner {
    /// New intervals apply to what is waiting: every pull request that was waiting for its next
    /// poll is fetched again now (failures keep counting), and from then on at the new pace.
    fn apply_new_settings(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        for entry in state.keys.values_mut() {
            if entry.next_due.is_some() {
                entry.next_due = Some(now);
            }
        }
    }

    /// The `[pull_requests] auto_fix_enabled` switch changed: every thread with an active link
    /// shows it (the bar's "Off by settings"), so each one is told to read its state again. The
    /// scheduler runs this, not `update_settings`, which is synchronous and may be called from
    /// outside the runtime. Archived and deleted threads show nothing and read it on restore.
    async fn announce_global_auto_fix(&self) {
        let mut after = 0;
        loop {
            let storage = self.storage.clone();
            let page = match self
                .blocking(move || storage.active_pull_request_links(after, PAGE))
                .await
            {
                Ok(Ok(page)) => page,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "auto fix switch change could not be announced");
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error, "auto fix switch change could not be announced");
                    return;
                }
            };
            let full = page.len() == PAGE;
            for link in page {
                after = link.link_id;
                self.announce_auto_fix(&link.thread_id, PullRequestAutoFixChange::Policy)
                    .await;
            }
            if !full {
                return;
            }
        }
    }

    /// One planning round: starts what is due and returns when the next thing is.
    async fn round(self: &Arc<Self>) -> Option<Instant> {
        let config = self.config();
        let planner = Arc::clone(self);
        let plan = match self.blocking(move || plan_blocking(&planner)).await {
            Ok(Ok(plan)) => plan,
            Ok(Err(error)) => {
                tracing::warn!(%error, "pull request tracking could not be planned");
                return Some(Instant::now() + Duration::from_secs(config.pending_poll_seconds));
            }
            Err(error) => {
                tracing::warn!(%error, "pull request tracking could not be planned");
                return Some(Instant::now() + Duration::from_secs(config.pending_poll_seconds));
            }
        };
        self.run_stage_hook(Stage::Planned);
        // Planning waited on the database and the ownership registry: tracking may have been
        // switched off meanwhile, and then nothing is started from this plan.
        if !self.config().enabled {
            return None;
        }
        let now = Instant::now();
        let mut earliest = self.coordinate(&plan.targets).await;
        let mut groups: HashMap<PullRequestKey, Vec<Target>> = HashMap::new();
        for target in plan.targets {
            groups.entry(target.key.clone()).or_default().push(target);
        }
        let mut start = Vec::new();
        let mut resolve = Vec::new();
        {
            let mut state = self.lock();
            // A key stays while a thread tracks it, a fetch of it is under way, or a child waits
            // for it: the failure state of a pull request is one, whoever asks.
            let mut needed: HashSet<PullRequestKey> = state.flights.keys().cloned().collect();
            needed.extend(groups.keys().cloned());
            needed.extend(plan.children.iter().map(|(_, key, _)| key.clone()));
            state.keys.retain(|key, _| needed.contains(key));
            for (key, targets) in groups {
                let forced = targets
                    .iter()
                    .any(|target| state.forced.contains(&target.thread_id));
                let (stored_seq, terminal) = plan.stored.get(&key).copied().unwrap_or((0, false));
                let entry = state
                    .keys
                    .entry(key.clone())
                    .or_insert_with(|| KeyState::new(now, terminal, stored_seq));
                // The shared snapshot is the other processes' word too: a merge or a reopening they
                // stored is honoured when it is newer than what this entry has adopted. An older
                // read (the plan is older than a fetch this process finished meanwhile) is not.
                if stored_seq > entry.adopted_seq {
                    entry.adopted_seq = stored_seq;
                    if entry.terminal != terminal {
                        entry.terminal = terminal;
                        entry.next_due = if terminal { None } else { Some(now) };
                    }
                }
                if forced {
                    entry.next_due = Some(now);
                }
                let next_due = entry.next_due;
                let interests: Vec<Interest> = targets.into_iter().map(Interest::Thread).collect();
                if let Some(flight) = state.flights.get(&key) {
                    flight.join(interests);
                    continue;
                }
                match next_due {
                    Some(due) if due <= now => start.push((key, interests)),
                    Some(due) => earliest = earlier(earliest, due),
                    None => {}
                }
            }
            state.forced.clear();
            let pending = Duration::from_secs(config.pending_poll_seconds);
            let live_children: HashSet<&String> =
                plan.children.iter().map(|(child, _, _)| child).collect();
            state
                .inheritance_due
                .retain(|child, _| live_children.contains(child));
            for (child, key, permit) in plan.children {
                if state.resolving.contains(&child) {
                    continue;
                }
                if let Some(due) = state.inheritance_due.get(&child)
                    && *due > now
                {
                    earliest = earlier(earliest, *due);
                    continue;
                }
                // A pull request that failed to be fetched is tried again when its backoff
                // ends, for the children that wait for it as for the threads that track it.
                if let Some(entry) = state.keys.get(&key)
                    && entry.failures > 0
                    && let Some(due) = entry.backoff_until
                    && due > now
                {
                    earliest = earlier(earliest, due);
                    continue;
                }
                state.resolving.insert(child.clone());
                state.inheritance_due.insert(child.clone(), now + pending);
                resolve.push((child, key, permit));
            }
        }
        // One fetch per pull request, started with every party that waits for it.
        let mut fetches: HashMap<PullRequestKey, Vec<Interest>> = start.into_iter().collect();
        for (child, key, permit) in &resolve {
            fetches
                .entry(key.clone())
                .or_default()
                .push(Interest::Child {
                    child: child.clone(),
                    permit: permit.clone(),
                });
        }
        let flights: HashMap<PullRequestKey, Arc<Flight>> = fetches
            .into_iter()
            .map(|(key, interests)| (key.clone(), self.start_or_join(key, interests)))
            .collect();
        for (child, key, permit) in resolve {
            if let Some(flight) = flights.get(&key) {
                self.spawn_resolution(child, permit, Arc::clone(flight));
            }
        }
        earliest
    }

    fn spawn_resolution(self: &Arc<Self>, child: String, permit: OwnerPermit, flight: Arc<Flight>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let result = inner.resolve_child_in(child.clone(), permit, flight).await;
            let pending = Duration::from_secs(inner.config().pending_poll_seconds);
            {
                let mut state = inner.lock();
                state.resolving.remove(&child);
                match &result {
                    Ok(InheritanceResolution::Pending(_)) | Err(_) => {
                        state
                            .inheritance_due
                            .insert(child, Instant::now() + pending);
                    }
                    Ok(_) => {
                        state.inheritance_due.remove(&child);
                    }
                }
            }
            if let Err(error) = &result {
                tracing::debug!(%error, "a pending pull request inheritance stays pending");
            }
            inner.wake.notify_one();
        });
    }
}
