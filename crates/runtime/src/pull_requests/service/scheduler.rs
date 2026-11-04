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

use storage::StorageError;
use storage::pull_requests::{InheritanceResolution, PullRequestKey, PullRequestState};
use tokio::time::Instant;

use super::flight::{Interest, Target};
use super::{Inner, KeyState};
use crate::ownership::OwnerPermit;

/// Rows read per page of a listing.
const PAGE: usize = 200;

/// Most pending inheritances looked at in one round.
const MAX_CHILDREN_PER_ROUND: usize = 1_000;

struct Plan {
    targets: Vec<Target>,
    /// Keys whose stored facts say merged or closed.
    terminal: HashSet<PullRequestKey>,
    children: Vec<(String, OwnerPermit)>,
}

pub(super) async fn run(inner: Arc<Inner>) {
    let mut settings = inner.settings.subscribe();
    let mut shutdown = inner.shutdown.subscribe();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let config = *settings.borrow_and_update();
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
    let mut terminal = HashSet::new();
    let mut seen = HashSet::new();
    for target in &targets {
        if !seen.insert(target.key.clone()) {
            continue;
        }
        let facts = inner
            .storage
            .pull_request_snapshot(&target.key)?
            .and_then(|snapshot| snapshot.facts);
        if facts.is_some_and(|facts| facts.state != PullRequestState::Open) {
            terminal.insert(target.key.clone());
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
                children.push((record.child_thread_id.clone(), permit));
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
        terminal,
        children,
    })
}

impl Inner {
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
        let now = Instant::now();
        let mut earliest = None;
        let mut groups: HashMap<PullRequestKey, Vec<Target>> = HashMap::new();
        for target in plan.targets {
            groups.entry(target.key.clone()).or_default().push(target);
        }
        let mut start = Vec::new();
        let mut resolve = Vec::new();
        {
            let mut state = self.lock();
            let flying: HashSet<PullRequestKey> = state.flights.keys().cloned().collect();
            state
                .keys
                .retain(|key, _| groups.contains_key(key) || flying.contains(key));
            for (key, targets) in groups {
                let forced = targets
                    .iter()
                    .any(|target| state.forced.contains(&target.thread_id));
                let terminal = plan.terminal.contains(&key);
                let entry = state.keys.entry(key.clone()).or_insert(KeyState {
                    next_due: if terminal { None } else { Some(now) },
                    failures: 0,
                });
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
                plan.children.iter().map(|(child, _)| child).collect();
            state
                .inheritance_due
                .retain(|child, _| live_children.contains(child));
            for (child, permit) in plan.children {
                if state.resolving.contains(&child) {
                    continue;
                }
                match state.inheritance_due.get(&child) {
                    Some(due) if *due > now => earliest = earlier(earliest, *due),
                    _ => {
                        state.resolving.insert(child.clone());
                        state.inheritance_due.insert(child.clone(), now + pending);
                        resolve.push((child, permit));
                    }
                }
            }
        }
        for (key, interests) in start {
            self.start_or_join(key, interests);
        }
        for (child, permit) in resolve {
            self.spawn_resolution(child, permit);
        }
        earliest
    }

    fn spawn_resolution(self: &Arc<Self>, child: String, permit: OwnerPermit) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let result = inner.resolve_child(child.clone(), permit).await;
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
