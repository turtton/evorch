//! Waiting until a page shows what the agent names (`browser_wait_for`).

use std::fmt;
use std::time::Duration;

use super::{Activity, PageAgent};
use crate::agent::{AgentError, PageEvents, snapshot};

/// What [`PageAgent::wait_for`] waits for. Every condition given must hold
/// at the same check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitFor {
    /// Text in the name or value of an element of the outline.
    pub text: Option<String>,
    pub text_gone: Option<String>,
    /// A ref (`eN`) of this or an earlier outline that the outline shows.
    pub reference: Option<String>,
    pub reference_gone: Option<String>,
    /// No request in flight.
    pub network_idle: bool,
    /// Text the page URL contains.
    pub url: Option<String>,
    /// A URL other than the one at the first check.
    pub url_changes: bool,
}

/// When a wait gives up. Events bound a busy page. A page that does nothing
/// sends no event, so time bounds the wait as well.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitLimits {
    pub events: u64,
    pub time: Duration,
}

impl Default for WaitLimits {
    fn default() -> Self {
        Self {
            events: 1000,
            time: crate::agent::REQUEST_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Met,
    EventLimit,
    TimeLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waited {
    pub outcome: WaitOutcome,
    /// The conditions that did not hold at the last check.
    pub unmet: Vec<String>,
    /// Page events seen while waiting.
    pub events: u64,
    /// How often the conditions were checked; 1 when they held at once.
    pub checks: u32,
}

enum Condition<'a> {
    Text(&'a str, bool),
    Ref(&'a str, i64, bool),
    NetworkIdle,
    Url(&'a str),
    UrlChanges,
}

impl fmt::Display for Condition<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let change = |shown: &bool| if *shown { "appear" } else { "disappear" };
        match self {
            Self::Text(text, shown) => write!(f, "text {text:?} to {}", change(shown)),
            Self::Ref(reference, _, shown) => write!(f, "{reference} to {}", change(shown)),
            Self::NetworkIdle => f.write_str("no request in flight"),
            Self::Url(part) => write!(f, "a URL containing {part:?}"),
            Self::UrlChanges => f.write_str("the URL to change"),
        }
    }
}

impl WaitFor {
    fn conditions(&self) -> Result<Vec<Condition<'_>>, AgentError> {
        fn reference(reference: &str, shown: bool) -> Result<Condition<'_>, AgentError> {
            reference
                .strip_prefix('e')
                .and_then(|id| id.parse().ok())
                .map(|id| Condition::Ref(reference, id, shown))
                .ok_or_else(|| AgentError::UnknownRef(reference.to_owned()))
        }
        let mut conditions = Vec::new();
        conditions.extend(self.text.as_deref().map(|text| Condition::Text(text, true)));
        conditions.extend(
            self.text_gone
                .as_deref()
                .map(|text| Condition::Text(text, false)),
        );
        if let Some(id) = &self.reference {
            conditions.push(reference(id, true)?);
        }
        if let Some(id) = &self.reference_gone {
            conditions.push(reference(id, false)?);
        }
        if self.network_idle {
            conditions.push(Condition::NetworkIdle);
        }
        conditions.extend(self.url.as_deref().map(Condition::Url));
        if self.url_changes {
            conditions.push(Condition::UrlChanges);
        }
        if conditions.is_empty() {
            return Err(AgentError::Refused(
                "name something to wait for: text, text_gone, ref, ref_gone, network_idle, url or url_changes".into(),
            ));
        }
        Ok(conditions)
    }
}

impl PageAgent {
    /// Checks the conditions, and again after each page event, until they
    /// hold or `limits` runs out. Giving up is an outcome, not an error.
    pub async fn wait_for(
        &self,
        wanted: &WaitFor,
        limits: WaitLimits,
    ) -> Result<Waited, AgentError> {
        let conditions = wanted.conditions()?;
        let label = conditions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let waited = self.wait(&conditions, limits, &label).await;
        let error = match &waited {
            Ok(waited) if waited.outcome == WaitOutcome::Met => None,
            Ok(waited) => Some(format!("gave up on {}", waited.unmet.join(", "))),
            Err(error) => Some(error.to_string()),
        };
        self.push_activity(Activity::Action {
            label: format!("wait for {label}"),
            error,
        });
        waited
    }

    async fn wait(
        &self,
        conditions: &[Condition<'_>],
        limits: WaitLimits,
        label: &str,
    ) -> Result<Waited, AgentError> {
        let mut events = self.target.page_events().await?.ok_or_else(|| {
            AgentError::Refused("this page reports no changes to wait for".into())
        })?;
        // A time too long to add means no time limit.
        let deadline = tokio::time::Instant::now().checked_add(limits.time);
        let first = events.borrow().count;
        let mut start_url = None;
        let mut checks = 0;
        loop {
            // Marks the value seen first, so an event during the check wakes
            // the next one.
            let seen = *events.borrow_and_update();
            checks += 1;
            let unmet = self.unmet(conditions, seen, &mut start_url).await?;
            let used = seen.count.saturating_sub(first);
            let outcome = if unmet.is_empty() {
                Some(WaitOutcome::Met)
            } else if used >= limits.events {
                Some(WaitOutcome::EventLimit)
            } else {
                None
            };
            if let Some(outcome) = outcome {
                return Ok(Waited {
                    outcome,
                    unmet,
                    events: used,
                    checks,
                });
            }
            if checks == 1 {
                self.push_activity(Activity::Action {
                    label: format!("waiting for {label}"),
                    error: None,
                });
            }
            let changed = match deadline {
                Some(deadline) => tokio::time::timeout_at(deadline, events.changed()).await,
                None => Ok(events.changed().await),
            };
            match changed {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(AgentError::Navigation("the page closed".into())),
                Err(_) => {
                    return Ok(Waited {
                        outcome: WaitOutcome::TimeLimit,
                        unmet,
                        events: events.borrow().count.saturating_sub(first),
                        checks,
                    });
                }
            }
        }
    }

    async fn unmet(
        &self,
        conditions: &[Condition<'_>],
        seen: PageEvents,
        start_url: &mut Option<String>,
    ) -> Result<Vec<String>, AgentError> {
        let reads_outline = conditions
            .iter()
            .any(|condition| matches!(condition, Condition::Text(..) | Condition::Ref(..)));
        let nodes = if reads_outline {
            self.outline_nodes().await?
        } else {
            Vec::new()
        };
        let refs = reads_outline.then(|| snapshot::render(&nodes).refs);
        let url = if conditions
            .iter()
            .any(|condition| matches!(condition, Condition::Url(_) | Condition::UrlChanges))
        {
            self.location().await?
        } else {
            None
        };
        let start = start_url.get_or_insert_with(|| url.clone().unwrap_or_default());
        Ok(conditions
            .iter()
            .filter(|condition| !match condition {
                Condition::Text(text, shown) => snapshot::mentions(&nodes, text) == *shown,
                Condition::Ref(_, id, shown) => {
                    refs.as_ref().is_some_and(|refs| refs.contains(id)) == *shown
                }
                Condition::NetworkIdle => seen.requests == 0,
                Condition::Url(part) => url.as_deref().is_some_and(|url| url.contains(part)),
                Condition::UrlChanges => url.as_deref().is_some_and(|url| url != start),
            })
            .map(|condition| match condition {
                Condition::NetworkIdle => format!("{condition} ({} now)", seen.requests),
                _ => condition.to_string(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tokio::sync::watch;

    use super::*;
    use crate::agent::CdpTarget;

    /// A page whose outline, URL and events the test sets.
    struct Scripted {
        texts: Mutex<Vec<(u64, &'static str)>>,
        url: Mutex<&'static str>,
        events: watch::Sender<PageEvents>,
    }

    impl Scripted {
        fn new(texts: &[(u64, &'static str)]) -> Arc<Self> {
            Arc::new(Self {
                texts: Mutex::new(texts.to_vec()),
                url: Mutex::new("https://example.test/"),
                events: watch::Sender::new(PageEvents::default()),
            })
        }

        /// Changes the page and reports one event.
        fn change(&self, edit: impl FnOnce(&Self, &mut PageEvents)) {
            self.events.send_modify(|seen| {
                edit(self, seen);
                seen.count += 1;
            });
        }
    }

    #[async_trait::async_trait]
    impl CdpTarget for Scripted {
        async fn call(&self, method: &str, _params: Value) -> Result<Value, AgentError> {
            Ok(match method {
                "Accessibility.getFullAXTree" => {
                    let nodes: Vec<Value> = self
                        .texts
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(id, text)| {
                            json!({
                                "nodeId": id.to_string(), "role": {"value": "button"},
                                "name": {"value": text}, "backendDOMNodeId": id,
                            })
                        })
                        .collect();
                    json!({"nodes": nodes})
                }
                "Runtime.evaluate" => json!({"result": {"value": *self.url.lock().unwrap()}}),
                _ => json!({}),
            })
        }

        async fn navigate(&self, _url: &str) -> Result<(), AgentError> {
            Ok(())
        }

        async fn page_events(&self) -> Result<Option<watch::Receiver<PageEvents>>, AgentError> {
            Ok(Some(self.events.subscribe()))
        }
    }

    const NO_TIME_LIMIT: WaitLimits = WaitLimits {
        events: 1000,
        time: Duration::MAX,
    };

    /// Resolves once the wait has checked once and found the page lacking.
    async fn waiting(page: &PageAgent) {
        page.activity_changes()
            .wait_for(|_| {
                page.activity().iter().any(|entry| {
                    matches!(entry, Activity::Action { label, .. } if label.starts_with("waiting for"))
                })
            })
            .await
            .unwrap();
    }

    fn spawn_wait(
        page: &Arc<PageAgent>,
        wanted: WaitFor,
        limits: WaitLimits,
    ) -> tokio::task::JoinHandle<Result<Waited, AgentError>> {
        let page = Arc::clone(page);
        tokio::spawn(async move { page.wait_for(&wanted, limits).await })
    }

    #[tokio::test]
    async fn a_wait_ends_on_the_event_that_brings_the_text_and_drops_the_ref() {
        // Given: a page showing e1 "Loading" and a wait for "Ready" and e1 to go.
        let target = Scripted::new(&[(1, "Loading")]);
        let page = Arc::new(PageAgent::new(target.clone()));
        let wanted = WaitFor {
            text: Some("Ready".into()),
            reference_gone: Some("e1".into()),
            ..Default::default()
        };
        let wait = spawn_wait(&page, wanted, NO_TIME_LIMIT);

        // When: the page changes after the wait looked at it.
        waiting(&page).await;
        target.change(|page, _| *page.texts.lock().unwrap() = vec![(2, "Ready")]);
        let waited = wait.await.unwrap().unwrap();

        // Then: the second check found both conditions.
        assert_eq!(
            waited,
            Waited {
                outcome: WaitOutcome::Met,
                unmet: Vec::new(),
                events: 1,
                checks: 2,
            }
        );
        assert!(matches!(
            page.activity().last(),
            Some(Activity::Action { label, error: None })
                if label == "wait for text \"Ready\" to appear, e1 to disappear"
        ));
    }

    #[tokio::test]
    async fn every_condition_must_hold_at_the_same_check() {
        // Given: a wait for a new URL with no request in flight.
        let target = Scripted::new(&[]);
        target.events.send_modify(|seen| seen.requests = 1);
        let page = Arc::new(PageAgent::new(target.clone()));
        let wanted = WaitFor {
            network_idle: true,
            url_changes: true,
            url: Some("/done".into()),
            ..Default::default()
        };
        let wait = spawn_wait(&page, wanted, NO_TIME_LIMIT);
        waiting(&page).await;

        // When: the URL changes while a request is in flight, then it ends.
        target.change(|page, _| *page.url.lock().unwrap() = "https://example.test/done");
        target.change(|_, seen| seen.requests = 0);
        let waited = wait.await.unwrap().unwrap();

        // Then: the wait ended only once both had happened.
        assert_eq!(waited.outcome, WaitOutcome::Met);
        assert_eq!(waited.events, 2);
        assert!(waited.checks >= 2, "{waited:?}");
    }

    #[tokio::test]
    async fn a_busy_page_ends_the_wait_after_its_event_budget() {
        // Given: a wait allowed three page events.
        let target = Scripted::new(&[(1, "Loading")]);
        let page = Arc::new(PageAgent::new(target.clone()));
        let wanted = WaitFor {
            text_gone: Some("Loading".into()),
            ..Default::default()
        };
        let limits = WaitLimits {
            events: 3,
            ..NO_TIME_LIMIT
        };
        let wait = spawn_wait(&page, wanted, limits);
        waiting(&page).await;

        // When: the page keeps changing without dropping the text.
        for _ in 0..3 {
            target.change(|_, _| {});
        }
        let waited = wait.await.unwrap().unwrap();

        // Then: the wait gave up and says what it still waited for.
        assert_eq!(waited.outcome, WaitOutcome::EventLimit);
        assert_eq!(waited.events, 3);
        assert_eq!(waited.unmet, ["text \"Loading\" to disappear"]);
        assert!(matches!(
            page.activity().last(),
            Some(Activity::Action { error: Some(error), .. })
                if error == "gave up on text \"Loading\" to disappear"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_page_ends_the_wait_at_the_time_limit() {
        // Given: a page with a request that never ends and sends no event.
        let target = Scripted::new(&[]);
        target.events.send_modify(|seen| seen.requests = 1);
        let page = PageAgent::new(target);

        // When: the agent waits for the network to go quiet (virtual time).
        let waited = page
            .wait_for(
                &WaitFor {
                    network_idle: true,
                    ..Default::default()
                },
                WaitLimits::default(),
            )
            .await
            .unwrap();

        // Then: the time limit ended it.
        assert_eq!(waited.outcome, WaitOutcome::TimeLimit);
        assert_eq!(waited.unmet, ["no request in flight (1 now)"]);
        assert_eq!(waited.checks, 1);
    }

    #[tokio::test]
    async fn waits_need_a_condition_a_valid_ref_and_page_events() {
        let page = PageAgent::new(Scripted::new(&[]));
        let nothing = page.wait_for(&WaitFor::default(), NO_TIME_LIMIT).await;
        let bad_ref = page
            .wait_for(
                &WaitFor {
                    reference: Some("button".into()),
                    ..Default::default()
                },
                NO_TIME_LIMIT,
            )
            .await;
        let silent = PageAgent::new(Arc::new(Silent))
            .wait_for(
                &WaitFor {
                    network_idle: true,
                    ..Default::default()
                },
                NO_TIME_LIMIT,
            )
            .await;

        assert!(matches!(nothing, Err(AgentError::Refused(_))));
        assert!(matches!(bad_ref, Err(AgentError::UnknownRef(_))));
        assert!(matches!(silent, Err(AgentError::Refused(_))));
    }

    /// A target that reports no page events, like CEF's.
    struct Silent;

    #[async_trait::async_trait]
    impl CdpTarget for Silent {
        async fn call(&self, _method: &str, _params: Value) -> Result<Value, AgentError> {
            Ok(json!({}))
        }

        async fn navigate(&self, _url: &str) -> Result<(), AgentError> {
            Ok(())
        }
    }
}
