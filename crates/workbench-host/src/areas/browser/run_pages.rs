//! Runtime pages use run-incarnation ownership, independent of client sessions.
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine, engine::general_purpose::STANDARD};
use browser::agent::{AgentError, CdpTarget, PageAgent};
use renderer_ipc::{OpenArtifact, Request, SessionId, TabId, Viewport};
use runtime::artifacts::{ArtifactPage, ArtifactPages, ArtifactRecord};
use serde_json::{Value, json};

use super::renderer::{Phase, RendererOwner, Tab};

#[derive(Clone)]
pub(crate) struct RunTab {
    pub incarnation: String,
    pub tab: Tab,
    pub artifact: Option<ArtifactRecord>,
}

#[derive(Default)]
struct PageState {
    pages: HashMap<String, Arc<Page>>,
    pending: HashMap<String, uuid::Uuid>,
}

pub struct RuntimePages {
    owner: Option<RendererOwner>,
    opening: tokio::sync::Mutex<()>,
    open_slots: tokio::sync::Semaphore,
    state: Mutex<PageState>,
}
impl Default for RuntimePages {
    fn default() -> Self {
        Self {
            owner: None,
            opening: tokio::sync::Mutex::new(()),
            open_slots: tokio::sync::Semaphore::new(renderer_ipc::MAX_TABS),
            state: Mutex::default(),
        }
    }
}

/// A close, replacement or cancelled future invalidates only this incarnation.
struct PendingOpen<'a> {
    state: &'a Mutex<PageState>,
    key: String,
    incarnation: uuid::Uuid,
}
impl PendingOpen<'_> {
    fn current(&self) -> bool {
        self.state.lock().unwrap().pending.get(&self.key) == Some(&self.incarnation)
    }
}
impl Drop for PendingOpen<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.pending.get(&self.key) == Some(&self.incarnation) {
            state.pending.remove(&self.key);
        }
    }
}
impl RuntimePages {
    pub fn new(owner: Option<RendererOwner>) -> Self {
        Self {
            owner,
            ..Self::default()
        }
    }
    pub fn artifact_of(&self, key: &str) -> Option<ArtifactRecord> {
        let owner = self.owner.as_ref()?;
        owner
            .run_tabs
            .lock()
            .unwrap()
            .get(key)
            .and_then(|run| run.artifact.clone())
    }
    fn prune(&self, owner: &RendererOwner, state: &mut PageState) {
        let status = owner.status();
        state.pages.retain(|_, page| {
            let valid = status.generation == page.tab.generation
                && matches!(status.phase, Phase::Ready | Phase::Idle);
            if !valid {
                page.close();
            }
            valid
        });
    }
    async fn open_page(
        &self,
        key: &str,
        html: Vec<u8>,
        artifact: Option<ArtifactRecord>,
    ) -> Result<Arc<dyn ArtifactPage>, String> {
        let owner = self.owner.as_ref().ok_or("core renderer is unavailable")?;
        // Bound queued bodies separately from both the spawn fence and tab budget.
        let _slot = self
            .open_slots
            .try_acquire()
            .map_err(|_| "renderer is busy")?;
        let (open, body) = OpenArtifact::with_files(
            SessionId(0),
            TabId(0),
            Viewport {
                width: 1024,
                height: 768,
            },
            30,
            30_000,
            vec![("index.html".into(), "text/html".into(), html)],
        );
        Request::OpenArtifact(open.clone())
            .validate(body.len())
            .map_err(|_| "invalid artifact page")?;
        let incarnation = uuid::Uuid::new_v4();
        {
            let mut state = self.state.lock().unwrap();
            self.prune(owner, &mut state);
            state.pending.insert(key.to_owned(), incarnation);
        }
        let pending = PendingOpen {
            state: &self.state,
            key: key.to_owned(),
            incarnation,
        };
        // Register BEFORE this await: close also cancels opens waiting their turn.
        let _opening = self.opening.lock().await;
        let old = {
            let mut state = self.state.lock().unwrap();
            if state.pending.get(key) != Some(&incarnation) {
                return Err("artifact page was closed or replaced".into());
            }
            state.pages.remove(key)
        };
        if let Some(old) = old {
            old.close();
            // Replacing the fourth tab must release its budget before opening.
            owner.close_wait(old.tab).await.map_err(|e| e.to_string())?;
        }
        if !pending.current() {
            return Err("artifact page was closed or replaced".into());
        }
        let tab = owner.open(open, body).await.map_err(|e| e.to_string())?;
        {
            let mut state = self.state.lock().unwrap();
            let status = owner.status();
            if state.pending.get(key) == Some(&incarnation)
                && status.generation == tab.generation
                && matches!(status.phase, Phase::Ready | Phase::Idle)
            {
                let closed = Arc::new(AtomicBool::new(false));
                let notices = Arc::new(Mutex::new(renderer_ipc::Notices::default()));
                let incarnation = incarnation.simple().to_string();
                let page = Arc::new(Page {
                    owner: owner.clone(),
                    key: key.to_owned(),
                    incarnation: incarnation.clone(),
                    tab,
                    closed: closed.clone(),
                    notices: notices.clone(),
                    agent: PageAgent::new(Arc::new(Target {
                        owner: owner.clone(),
                        tab,
                        closed,
                        notices,
                    })),
                });
                owner.run_tabs.lock().unwrap().insert(
                    key.to_owned(),
                    RunTab {
                        incarnation,
                        tab,
                        artifact,
                    },
                );
                state.pages.insert(key.to_owned(), page.clone());
                state.pending.remove(key);
                return Ok(page);
            }
        }
        owner.close_wait(tab).await.map_err(|e| e.to_string())?;
        Err("artifact page was closed or replaced".into())
    }
}
#[async_trait::async_trait]
impl ArtifactPages for RuntimePages {
    async fn open(&self, key: &str, html: Vec<u8>) -> Result<Arc<dyn ArtifactPage>, String> {
        self.open_page(key, html, None).await
    }
    async fn open_artifact(
        &self,
        key: &str,
        html: Vec<u8>,
        record: &ArtifactRecord,
    ) -> Result<Arc<dyn ArtifactPage>, String> {
        self.open_page(key, html, Some(record.clone())).await
    }
    fn page(&self, key: &str) -> Option<Arc<dyn ArtifactPage>> {
        let owner = self.owner.as_ref()?;
        let mut state = self.state.lock().unwrap();
        self.prune(owner, &mut state);
        state
            .pages
            .get(key)
            .cloned()
            .map(|page| page as Arc<dyn ArtifactPage>)
    }
    fn close(&self, key: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        state.pending.remove(key);
        if let Some(page) = state.pages.remove(key) {
            page.close();
            true
        } else {
            false
        }
    }
}
struct Page {
    owner: RendererOwner,
    key: String,
    incarnation: String,
    tab: Tab,
    closed: Arc<AtomicBool>,
    notices: Arc<Mutex<renderer_ipc::Notices>>,
    agent: PageAgent,
}
impl Page {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut tabs = self.owner.run_tabs.lock().unwrap();
        if tabs
            .get(&self.key)
            .is_some_and(|entry| entry.incarnation == self.incarnation)
        {
            tabs.remove(&self.key);
        }
        drop(tabs);
        self.owner.close_later(self.tab);
    }
}
impl ArtifactPage for Page {
    fn agent(&self) -> &PageAgent {
        &self.agent
    }
    fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.notices.lock().unwrap().blocked)
    }
    fn take_dialogs(&self) -> Vec<String> {
        std::mem::take(&mut self.notices.lock().unwrap().dialogs)
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        self.close();
    }
}
struct Target {
    owner: RendererOwner,
    tab: Tab,
    closed: Arc<AtomicBool>,
    notices: Arc<Mutex<renderer_ipc::Notices>>,
}
impl Target {
    fn check(&self) -> Result<(), AgentError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(AgentError::Refused("artifact page is closed".into()));
        }
        Ok(())
    }
    async fn collect_notices(&self) -> Result<(), AgentError> {
        let notices = self.owner.notices(self.tab).await.map_err(error)?;
        let mut saved = self.notices.lock().unwrap();
        saved.blocked.extend(notices.blocked);
        saved.dialogs.extend(notices.dialogs);
        *saved = std::mem::take(&mut *saved).bounded();
        Ok(())
    }
}
#[async_trait::async_trait]
impl CdpTarget for Target {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.check()?;
        let result = if method == "Page.captureScreenshot" {
            // Runtime screenshots retain the existing image bound, not the
            // smaller public CDP JSON limit. Use the dedicated body channel.
            json!({"data": STANDARD.encode(self.owner.capture(self.tab).await.map_err(error)?)})
        } else {
            self.owner
                .cdp(self.tab, method.to_owned(), params)
                .await
                .map_err(error)?
        };
        self.collect_notices().await?;
        Ok(result)
    }
    async fn navigate(&self, _url: &str) -> Result<(), AgentError> {
        Err(AgentError::Refused(
            "offline artifact navigation is not permitted".into(),
        ))
    }
    async fn settle(&self) -> Result<(), AgentError> {
        self.check()?;
        self.owner.settle(self.tab).await.map_err(error)?;
        self.collect_notices().await
    }
}
fn error(error: super::renderer::OwnerError) -> AgentError {
    AgentError::Refused(error.to_string())
}
