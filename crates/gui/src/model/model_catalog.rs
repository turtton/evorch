use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use catalog::{ModelCatalog, RefreshOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogRequest {
    Load,
    ForceRefresh,
}

/// What a catalog request actually did, so callers need not infer it from the cache file.
pub enum CatalogFetch {
    /// Cached data was read; no refresh was forced. A stale cache schedules a background one.
    Loaded(ModelCatalog),
    /// models.dev was fetched and the cache replaced.
    Refreshed(ModelCatalog),
    /// The forced refresh failed or was busy; the existing cache keeps being used.
    CacheRetained {
        catalog: ModelCatalog,
        reason: String,
    },
}

#[async_trait::async_trait]
pub trait CatalogBackend: Send + Sync {
    async fn fetch(&self, request: CatalogRequest, dir: &Path) -> Result<CatalogFetch, String>;
}

struct ModelsDev;

#[async_trait::async_trait]
impl CatalogBackend for ModelsDev {
    async fn fetch(&self, request: CatalogRequest, dir: &Path) -> Result<CatalogFetch, String> {
        match request {
            CatalogRequest::Load => ModelCatalog::load_or_refresh(dir)
                .await
                .map(CatalogFetch::Loaded),
            CatalogRequest::ForceRefresh => {
                ModelCatalog::force_refresh(dir)
                    .await
                    .map(|outcome| match outcome {
                        RefreshOutcome::Refreshed(catalog) => CatalogFetch::Refreshed(catalog),
                        RefreshOutcome::CacheRetained { catalog, reason } => {
                            CatalogFetch::CacheRetained {
                                catalog,
                                reason: reason.to_string(),
                            }
                        }
                    })
            }
        }
        .map_err(|error| error.to_string())
    }
}

struct Update {
    result: Result<Arc<ModelCatalog>, String>,
    busy: bool,
    warning: Option<String>,
    /// New data that running models should learn about: a successful fetch, not a cache read.
    publish: bool,
}

pub struct CatalogState {
    pub catalog: Option<Arc<ModelCatalog>>,
    /// When the data in `catalog` was fetched from models.dev.
    pub fetched_at: Option<SystemTime>,
    pub error: Option<String>,
    cache_dir: PathBuf,
    backend: Arc<dyn CatalogBackend>,
    rx: Option<mpsc::Receiver<Update>>,
    /// Newly fetched data not yet handed to the models; kept by whichever poll saw it first.
    published: Option<Arc<ModelCatalog>>,
}

impl std::fmt::Debug for CatalogState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogState")
            .field("fetched_at", &self.fetched_at)
            .field("error", &self.error)
            .field("busy", &self.is_busy())
            .finish_non_exhaustive()
    }
}

impl Default for CatalogState {
    fn default() -> Self {
        Self::with_backend(catalog::default_cache_dir(), Arc::new(ModelsDev))
    }
}

impl CatalogState {
    pub fn with_backend(cache_dir: PathBuf, backend: Arc<dyn CatalogBackend>) -> Self {
        Self {
            catalog: None,
            fetched_at: None,
            error: None,
            cache_dir,
            backend,
            rx: None,
            published: None,
        }
    }

    pub const fn is_busy(&self) -> bool {
        self.rx.is_some()
    }

    pub fn start(&mut self, request: CatalogRequest) {
        if self.is_busy() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let dir = self.cache_dir.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let runtime = match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(Update::failed(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async {
                let (mut catalog, warning, publish) = match backend.fetch(request, &dir).await {
                    Ok(CatalogFetch::Loaded(catalog)) => (catalog, None, false),
                    Ok(CatalogFetch::Refreshed(catalog)) => (catalog, None, true),
                    Ok(CatalogFetch::CacheRetained { catalog, reason }) => (
                        catalog,
                        Some(format!(
                            "Catalog was not updated; using cached data ({reason})"
                        )),
                        false,
                    ),
                    Err(error) => {
                        let _ = tx.send(Update::failed(error));
                        return;
                    }
                };
                let refresh = catalog.take_refresh();
                let _ = tx.send(Update {
                    result: Ok(Arc::new(catalog)),
                    busy: refresh.is_some(),
                    warning,
                    publish,
                });
                if let Some(refresh) = refresh {
                    let update = match refresh.await {
                        Ok(Ok(catalog)) => Update {
                            result: Ok(Arc::new(catalog)),
                            busy: false,
                            warning: None,
                            publish: true,
                        },
                        Ok(Err(error)) => Update::failed(error.to_string()),
                        Err(error) => Update::failed(error.to_string()),
                    };
                    let _ = tx.send(update);
                }
            });
        });
    }

    /// Applies finished worker results. Returns whether anything changed.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.take() else {
            return false;
        };
        let received = rx.try_recv();
        if matches!(received, Err(mpsc::TryRecvError::Empty)) {
            self.rx = Some(rx);
            return false;
        }
        self.receive(rx, received.map_err(|_| mpsc::RecvError));
        true
    }

    /// Blocks until the running request, including a background refresh it scheduled, has
    /// been applied. For callers without a frame loop (tests, tools); the GUI uses `poll`.
    pub fn wait_until_idle(&mut self) {
        while let Some(rx) = self.rx.take() {
            let received = rx.recv();
            self.receive(rx, received);
        }
    }

    fn receive(&mut self, rx: mpsc::Receiver<Update>, received: Result<Update, mpsc::RecvError>) {
        match received {
            Ok(update) => {
                match update.result {
                    Ok(catalog) => {
                        self.fetched_at = Some(catalog.fetched_at());
                        if update.publish {
                            self.published = Some(Arc::clone(&catalog));
                        }
                        self.catalog = Some(catalog);
                        self.error = update.warning;
                    }
                    Err(error) => self.error = Some(error),
                }
                if update.busy {
                    self.rx = Some(rx);
                }
            }
            Err(mpsc::RecvError) => {
                self.error = Some("Catalog worker stopped without a result".into());
            }
        }
    }

    /// Hands out newly fetched data exactly once, whichever poll noticed it, so the frame
    /// can publish it to the running models even when the settings view has been closed.
    pub fn take_published(&mut self) -> Option<Arc<ModelCatalog>> {
        self.published.take()
    }
}

impl Update {
    fn failed(error: String) -> Self {
        Self {
            result: Err(error),
            busy: false,
            warning: None,
            publish: false,
        }
    }
}
