//! The single upstream shared by native observation and legacy screencasts.
use super::*;
use source::{Frame, ObservedTab};
use std::sync::Weak;

impl TabPage {
    fn execution(&self) -> Option<ExecutionPage> {
        let creation = self.creation.as_ref()?;
        Some(ExecutionPage {
            stamp: creation.stamp.clone()?,
            browser_generation: creation.browser_generation,
            page_generation: creation.page_generation,
        })
    }
}
impl AgentBrowser {
    /// Existing, stamped pages only. This never starts Chromium or creates a page.
    pub fn observation_pages(&self) -> Vec<ExecutionPage> {
        let registry = lock(&self.shared.registry);
        registry
            .keys()
            .iter()
            .filter_map(|key| registry.active(key)?.1.execution())
            .take(32)
            .collect()
    }
    pub fn observation_current(&self, execution: &ExecutionPage) -> bool {
        self.shared
            .active(&execution.stamp.run)
            .and_then(|(_, page)| page.execution())
            .as_ref()
            == Some(execution)
    }
    pub fn observation_snapshot(&self, execution: &ExecutionPage) -> Option<Arc<Publication>> {
        let (_, page) = self.shared.active(&execution.stamp.run)?;
        let publication = self
            .shared
            .publication(&execution.stamp.run, &page.creation, None, 0)?;
        (publication.execution.as_ref() == Some(execution)).then(|| Arc::new(publication))
    }
    pub fn observe(&self, execution: &ExecutionPage) -> Result<Feed, AgentError> {
        self.shared.feed(&execution.stamp.run, Some(execution))
    }
}
impl Shared {
    fn publication(
        &self,
        key: &str,
        creation: &Option<Arc<Creation>>,
        frame: Option<Arc<Frame>>,
        revision: u64,
    ) -> Option<Publication> {
        let registry = lock(&self.registry);
        let (_, active) = registry.active(key)?;
        // Bind the lookup to the producer's immutable creation under the same
        // lock as metadata/pixels. A prior current() check is not a lease.
        if !same_creation(creation, &active.creation) {
            return None;
        }
        let targets = lock(&self.targets);
        let mut tabs: Vec<_> = registry
            .tabs(key)
            .into_iter()
            .filter_map(|listed| {
                let page = targets.get(&listed.target)?.upgrade()?;
                let (title, url) = lock(&page.metadata).clone();
                Some(ObservedTab {
                    id: page.id.clone(),
                    title,
                    url,
                })
            })
            .take(32)
            .collect();
        // Even if a run exceeds the observation limit, retain its active tab.
        if !tabs.iter().any(|tab| tab.id == active.id) {
            let (title, url) = lock(&active.metadata).clone();
            tabs.truncate(31);
            tabs.push(ObservedTab {
                id: active.id.clone(),
                title,
                url,
            });
        }
        let frame = frame.filter(|frame| frame.tab == active.id);
        let activity = active
            .agent
            .activity()
            .into_iter()
            .map(|entry| match entry {
                super::super::Activity::Action { label, error } => super::super::Activity::Action {
                    label: source::bounded(&label),
                    error: error.map(|e| source::bounded(&e)),
                },
                super::super::Activity::Blocked(s) => {
                    super::super::Activity::Blocked(source::bounded(&s))
                }
                super::super::Activity::Dialog(s) => {
                    super::super::Activity::Dialog(source::bounded(&s))
                }
            })
            .collect();
        Some(Publication {
            execution: active.execution(),
            revision,
            tabs,
            active_tab: Some(active.id.clone()),
            activity,
            frame,
        })
    }
    pub(super) fn feed(
        self: &Arc<Self>,
        key: &str,
        expected: Option<&ExecutionPage>,
    ) -> Result<Feed, AgentError> {
        let (_, page) = self
            .active(key)
            .ok_or_else(|| AgentError::NoPage(key.to_owned()))?;
        if expected.is_some_and(|expected| page.execution().as_ref() != Some(expected)) {
            return Err(AgentError::NoPage(key.to_owned()));
        }
        let mut producers = lock(&self.producers);
        producers.retain(|_, weak| weak.strong_count() > 0);
        let producer_key = format!(
            "{}:{}:{}",
            key,
            page.creation.as_ref().map_or(0, |c| c.browser_generation),
            page.creation.as_ref().map_or(0, |c| c.page_generation)
        );
        if let Some(inner) = producers
            .get(&producer_key)
            .and_then(Weak::upgrade)
            .filter(|inner| inner.updates.borrow().is_some())
        {
            return Ok(Feed {
                updates: inner.updates.clone(),
                inner,
            });
        }
        let initial = self.publication(key, &page.creation, None, 0).map(Arc::new);
        let (updates, receiver) = watch::channel(initial);
        let inner = Arc::new(FeedInner {
            updates: receiver.clone(),
            task: Mutex::new(None),
        });
        producers.insert(producer_key, Arc::downgrade(&inner));
        let shared = self.clone();
        let key = key.to_owned();
        let task = tokio::spawn(async move {
            shared.produce(&key, page, &updates).await;
            updates.send_replace(None);
        });
        *lock(&inner.task) = Some(task);
        Ok(Feed {
            inner,
            updates: receiver,
        })
    }
    async fn produce(
        self: &Arc<Self>,
        key: &str,
        initial: Arc<TabPage>,
        updates: &watch::Sender<Option<Arc<Publication>>>,
    ) {
        let creation = initial.creation.clone();
        let mut changes = self.changes.subscribe();
        let mut revision = 0;
        let mut sequence = 0;
        let mut last_frame = None;
        // The tab whose screencast start was refused and the retries spent.
        let mut refused: (Option<String>, usize) = (None, 0);
        loop {
            let Some((_, tab)) = self.active(key) else {
                break;
            };
            if !same_creation(&creation, &tab.creation) {
                break;
            }
            // Publish new tab identity without the previous tab's pixels before
            // any await. A concurrent switch is checked again on every publish.
            revision += 1;
            if let Some(page) = self.publication(key, &creation, last_frame.take(), revision) {
                last_frame = page.frame.clone();
                updates.send_replace(Some(Arc::new(page)));
            }
            let gate = tokio::select! { biased;
                _ = updates.closed() => break,
                gate = tab.streaming.clone().lock_owned() => gate,
            };
            if !self
                .active(key)
                .is_some_and(|(_, now)| Arc::ptr_eq(&now, &tab))
            {
                drop(gate);
                continue;
            }
            let mut incoming = tab.connection.frames(&tab.session);
            let _ = incoming.borrow_and_update();
            let started = tab.call("Page.startScreencast", json!({"format": "jpeg", "quality": 60, "maxWidth": WINDOW[0], "maxHeight": WINDOW[1]})).await;
            if let Err(error) = started {
                // No stream is live: pixels held from before are not paired
                // with the metadata published from here on.
                last_frame = None;
                drop(gate);
                if !start_refusal_is_transient(&error) {
                    tracing::debug!(%error, "agent observation cannot start a screencast");
                    break;
                }
                if refused.0.as_deref() != Some(tab.id.as_str()) {
                    refused = (Some(tab.id.clone()), 0);
                }
                let Some(delay) = START_RETRY_DELAYS.get(refused.1) else {
                    tracing::warn!(%error, "agent observation gave up starting a screencast");
                    break;
                };
                refused.1 += 1;
                if !self
                    .wait_to_retry(key, &tab, *delay, &mut changes, updates)
                    .await
                {
                    break;
                }
                continue;
            }
            refused = (None, 0);
            let mut activity = tab.agent.activity_changes();
            let mut finished = false;
            loop {
                tokio::select! { biased;
                    _ = updates.closed() => { finished = true; break; }
                    changed = changes.changed() => {
                        if changed.is_err() { finished = true; break; }
                    }
                    changed = activity.changed() => {
                        if changed.is_err() { finished = true; break; }
                    }
                    changed = incoming.changed() => {
                        if changed.is_err() { finished = true; break; }
                        let frame = incoming.borrow_and_update().clone();
                        if let Some(frame) = frame
                            && let Ok(image) = crate::jpeg::decode_jpeg(frame["data"].as_str().unwrap_or_default(), WINDOW) {
                                sequence += 1;
                                last_frame = Some(Arc::new(Frame { tab: tab.id.clone(), sequence, width: image.width(), height: image.height(), rgba: image.into_raw().into() }));
                        }
                    }
                }
                if !self
                    .active(key)
                    .is_some_and(|(_, now)| Arc::ptr_eq(&now, &tab))
                {
                    last_frame = None;
                    break;
                }
                revision += 1;
                if let Some(page) = self.publication(key, &creation, last_frame.clone(), revision) {
                    // publication checks tab/pixels under the same registry lock.
                    updates.send_replace(Some(Arc::new(page)));
                }
            }
            // Hold the gate until stop completes: an old consumer can never
            // stop a replacement producer. No close/release operation occurs.
            let _ = tab.raw("Page.stopScreencast", json!({})).await;
            drop(gate);
            if finished {
                break;
            }
        }
    }
}
/// Pause before each retry of a refused `Page.startScreencast` of one tab;
/// the schedule is also the bound (about 5 s), after which the producer ends.
const START_RETRY_DELAYS: [Duration; 8] = [
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
    Duration::from_millis(320),
    Duration::from_millis(640),
    Duration::from_millis(1280),
    Duration::from_millis(2560),
];

/// A page that navigates can refuse the start ("Not attached to an active
/// page") for a moment. Only a lost connection or a page that is gone cannot
/// recover; the latter shows as no active tab on the next pass.
fn start_refusal_is_transient(error: &AgentError) -> bool {
    matches!(error, AgentError::Cdp { detail, .. } if detail != crate::agent::connection::CONNECTION_CLOSED)
}

impl Shared {
    /// Waits `delay` before the next attempt without holding the streaming
    /// lease. `false` when every consumer is gone; `true` when the delay is
    /// over or the run's active tab is no longer `tab`.
    async fn wait_to_retry(
        &self,
        key: &str,
        tab: &Arc<TabPage>,
        delay: Duration,
        changes: &mut watch::Receiver<u64>,
        updates: &watch::Sender<Option<Arc<Publication>>>,
    ) -> bool {
        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);
        loop {
            tokio::select! { biased;
                _ = updates.closed() => return false,
                _ = &mut timer => return true,
                changed = changes.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    if !self.active(key).is_some_and(|(_, now)| Arc::ptr_eq(&now, tab)) {
                        return true;
                    }
                }
            }
        }
    }
}

fn same_creation(a: &Option<Arc<Creation>>, b: &Option<Arc<Creation>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
