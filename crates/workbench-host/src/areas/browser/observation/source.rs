//! Adapter over bootstrap's browser; never constructs a second owner.
use super::*;
use browser::agent::observation::{
    ExecutionStamp,
    source::{ExecutionPage, Publication},
};
use std::{collections::HashMap, sync::Mutex};
#[derive(Clone)]
pub struct AgentObservationSource {
    inner: Arc<Inner>,
    tasks: crate::areas::AreaTasks,
}
struct Inner {
    browser: Arc<browser::agent::AgentBrowser>,
    runtime: runtime::runtime::BrowserExecutionReader,
    core_epoch: u64,
    feeds: Mutex<HashMap<ObservationScope, watch::Sender<Option<Arc<ObservedPage>>>>>,
}
impl AgentObservationSource {
    pub fn new(
        browser: Arc<browser::agent::AgentBrowser>,
        runtime: runtime::runtime::BrowserExecutionReader,
        core_epoch: u64,
        tasks: crate::areas::AreaTasks,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                browser,
                runtime,
                core_epoch,
                feeds: Mutex::default(),
            }),
            tasks,
        }
    }
}
fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn execution(scope: &ObservationScope) -> ExecutionPage {
    ExecutionPage {
        stamp: Arc::new(ExecutionStamp {
            run: scope.run.as_str().into(),
            incarnation: scope.incarnation.as_str().into(),
        }),
        browser_generation: scope.browser_generation,
        page_generation: scope.page_generation,
    }
}
impl Inner {
    fn current(&self, scope: &ObservationScope) -> bool {
        let Ok(run) = scope.run.as_str().parse::<runtime::RunId>() else {
            return false;
        };
        scope.core_epoch == self.core_epoch
            && self.runtime.affiliation(run)
                == Some((scope.project.as_str().into(), scope.thread.as_str().into()))
            && self
                .runtime
                .observe(run)
                .is_some_and(|now| now.current() && now.incarnation == scope.incarnation.as_str())
            && self.browser.observation_current(&execution(scope))
    }
}
impl AgentObservationPort for AgentObservationSource {
    fn list(
        &self,
        project: &ProjectId,
        thread: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure> {
        let mut scopes = Vec::new();
        for page in self.inner.browser.observation_pages() {
            let (Ok(run), Ok(incarnation)) = (
                workbench_protocol::ids::RunId::new(page.stamp.run.clone()),
                workbench_protocol::ids::RunId::new(page.stamp.incarnation.clone()),
            ) else {
                continue;
            };
            let scope = ObservationScope {
                project: project.clone(),
                thread: thread.clone(),
                run,
                incarnation,
                core_epoch: self.inner.core_epoch,
                browser_generation: page.browser_generation,
                page_generation: page.page_generation,
            };
            if self.inner.current(&scope) {
                scopes.push(scope);
            }
            if scopes.len() == MAX_RUNS {
                break;
            }
        }
        if (ObservationReply::Runs {
            scopes: scopes.clone(),
        })
        .validate()
        .is_err()
        {
            return Err(ObservationFailure::Capacity);
        }
        Ok(scopes)
    }
    fn snapshot(&self, scope: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        if !self.inner.current(scope) {
            return Err(ObservationFailure::Revoked);
        }
        let cached = locked(&self.inner.feeds)
            .get(scope)
            .and_then(|tx| tx.borrow().clone());
        let page = match cached {
            Some(page) => page,
            None => convert(
                scope,
                self.inner
                    .browser
                    .observation_snapshot(&execution(scope))
                    .ok_or(ObservationFailure::Revoked)?
                    .as_ref(),
            )?,
        };
        if !self.inner.current(scope) {
            return Err(ObservationFailure::Revoked);
        }
        Ok(page)
    }
    fn subscribe(&self, scope: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
        let initial = self.snapshot(scope)?;
        let mut feeds = locked(&self.inner.feeds);
        if let Some(tx) = feeds.get(scope)
            && tx.borrow().is_some()
        {
            return Ok(ObservationFeed {
                updates: tx.subscribe(),
            });
        }
        if feeds.len() >= MAX_RUNS {
            return Err(ObservationFailure::Capacity);
        }
        let (tx, updates) = watch::channel(Some(initial));
        let inner = self.inner.clone();
        let scope = scope.clone();
        let worker_tx = tx.clone();
        let worker_scope = scope.clone();
        self.tasks.spawn("agent-observation-source", move |mut stop| async move {
            let tx = worker_tx;
            let scope = worker_scope;
            // Acquire the upstream only after task admission. Full/closed task
            // registries must not leave an untracked producer behind.
            let feed = match inner.browser.observe(&execution(&scope)) {
                Ok(feed) => feed,
                Err(_) => {
                    tx.send_replace(None);
                    let mut feeds = locked(&inner.feeds);
                    if feeds.get(&scope).is_some_and(|current| current.same_channel(&tx)) { feeds.remove(&scope); }
                    return Ok(());
                }
            };
            let mut source_updates = feed.updates.clone();
            let mut observed = scope.run.as_str().parse::<runtime::RunId>().ok().and_then(|run| inner.runtime.observe(run));
            loop {
                // Replay the latest value before waiting: an existing producer
                // may already hold the only frame of a static page.
                if !inner.current(&scope) { break; }
                let next = source_updates.borrow_and_update().clone();
                let Some(next) = next else { break; };
                let Ok(page) = convert(&scope, &next) else { break; };
                if !inner.current(&scope) { break; }
                tx.send_replace(Some(page));
                tokio::select! { biased;
                    _ = stop.stopped() => break,
                    _ = tx.closed() => break,
                    _ = async { if let Some(observed) = &mut observed { observed.invalidated().await; } } => break,
                    changed = source_updates.changed() => { if changed.is_err() { break; } }
                }
            }
            tx.send_replace(None);
            {
                let mut feeds = locked(&inner.feeds);
                if feeds.get(&scope).is_some_and(|current| current.same_channel(&tx)) { feeds.remove(&scope); }
            }
            drop(source_updates);
            feed.close().await;
            Ok(())
        }).map_err(|_| ObservationFailure::Capacity)?;
        feeds.insert(scope, tx);
        Ok(ObservationFeed { updates })
    }
    fn current(&self, scope: &ObservationScope) -> bool {
        self.inner.current(scope)
    }
}
fn convert(
    scope: &ObservationScope,
    page: &Publication,
) -> Result<Arc<ObservedPage>, ObservationFailure> {
    if page.execution.as_ref() != Some(&execution(scope)) {
        return Err(ObservationFailure::Revoked);
    }
    let id = |s: &str| ObservationId::new(s).map_err(|_| ObservationFailure::Unavailable);
    let mut snapshot = ObservationSnapshot {
        scope: scope.clone(),
        revision: page.revision,
        state: ObservationState::Live,
        tabs: page
            .tabs
            .iter()
            .take(MAX_TABS)
            .map(|tab| {
                Ok(ObservationTab {
                    id: id(&tab.id)?,
                    title: bounded(&tab.title),
                    url: bounded(&tab.url),
                })
            })
            .collect::<Result<_, ObservationFailure>>()?,
        active_tab: page.active_tab.as_deref().map(id).transpose()?,
        activity: page
            .activity
            .iter()
            .rev()
            .take(MAX_ACTIVITY)
            .rev()
            .map(|a| match a {
                browser::agent::Activity::Action { label, error } => ObservationActivity::Action {
                    label: bounded(label),
                    error: error.as_deref().map(bounded),
                },
                browser::agent::Activity::Blocked(s) => ObservationActivity::Blocked(bounded(s)),
                browser::agent::Activity::Dialog(s) => ObservationActivity::Dialog(bounded(s)),
            })
            .collect(),
    };
    // Budget the JSON envelope too (escaping + wrapper), not just each string.
    while ObservationReply::Snapshot(Box::new(snapshot.clone()))
        .validate()
        .is_err()
    {
        if !snapshot.activity.is_empty() {
            snapshot.activity.remove(0);
        } else if let Some(index) = snapshot
            .tabs
            .iter()
            .position(|tab| Some(&tab.id) != snapshot.active_tab.as_ref())
        {
            snapshot.tabs.remove(index);
        } else {
            return Err(ObservationFailure::Capacity);
        }
    }
    let frame = page
        .frame
        .as_ref()
        .map(|frame| {
            Ok::<_, ObservationFailure>(Arc::new(ObservedFrame {
                start: data::AgentFrameStart {
                    scope: scope.clone(),
                    tab: id(&frame.tab)?,
                    sequence: frame.sequence,
                    width: frame.width,
                    height: frame.height,
                    stride: frame
                        .width
                        .checked_mul(4)
                        .ok_or(ObservationFailure::Unavailable)?,
                    total_bytes: frame.rgba.len() as u64,
                },
                rgba: frame.rgba.clone(),
            }))
        })
        .transpose()?;
    let result = Arc::new(ObservedPage { snapshot, frame });
    if !result.validate() {
        return Err(ObservationFailure::Unavailable);
    }
    Ok(result)
}
fn bounded(s: &str) -> String {
    let mut n = s.len().min(MAX_TEXT_BYTES);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].into()
}

#[cfg(test)]
mod tests;
