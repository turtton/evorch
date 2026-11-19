//! Read-only, coherent browser publications. No page operation capability is
//! carried across this boundary. Resource bounds here are not redaction.
use super::ExecutionStamp;
use crate::agent::Activity;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPage {
    pub stamp: Arc<ExecutionStamp>,
    pub browser_generation: u64,
    pub page_generation: u64,
}
#[derive(Clone, Debug)]
pub struct ObservedTab {
    pub id: String,
    pub title: String,
    pub url: String,
}
#[derive(Clone)]
pub struct Frame {
    pub tab: String,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}
#[derive(Clone)]
pub struct Publication {
    pub execution: Option<ExecutionPage>,
    pub revision: u64,
    pub tabs: Vec<ObservedTab>,
    pub active_tab: Option<String>,
    pub activity: Vec<Activity>,
    pub frame: Option<Arc<Frame>>,
}
/// A shared upstream. Drop releases this consumer only. Last-consumer close
/// waits for the upstream's stop command and task completion.
#[derive(Clone)]
pub struct Feed {
    pub(crate) inner: Arc<FeedInner>,
    pub updates: watch::Receiver<Option<Arc<Publication>>>,
}
pub(crate) struct FeedInner {
    pub updates: watch::Receiver<Option<Arc<Publication>>>,
    pub task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Feed {
    pub async fn close(self) {
        let Self { inner, updates } = self;
        drop(updates);
        if let Ok(inner) = Arc::try_unwrap(inner) {
            drop(inner.updates);
            let task = inner
                .task
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(task) = task {
                let _ = task.await;
            }
        }
    }
}
pub(crate) fn bounded(s: &str) -> String {
    let mut n = s.len().min(2048);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_owned()
}
