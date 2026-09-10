use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::SystemTime;

use catalog::ModelCatalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogRequest {
    Load,
    ForceRefresh,
}

#[async_trait::async_trait]
pub trait CatalogBackend: Send + Sync {
    async fn fetch(&self, request: CatalogRequest, dir: &Path) -> Result<ModelCatalog, String>;
}

struct ModelsDev;

#[async_trait::async_trait]
impl CatalogBackend for ModelsDev {
    async fn fetch(&self, request: CatalogRequest, dir: &Path) -> Result<ModelCatalog, String> {
        match request {
            CatalogRequest::Load => ModelCatalog::load_or_refresh(dir).await,
            CatalogRequest::ForceRefresh => ModelCatalog::force_refresh(dir).await,
        }
        .map_err(|error| error.to_string())
    }
}

struct Update {
    result: Result<Arc<ModelCatalog>, String>,
    modified: Option<SystemTime>,
    busy: bool,
    warning: Option<String>,
}

pub struct CatalogState {
    pub catalog: Option<Arc<ModelCatalog>>,
    pub modified: Option<SystemTime>,
    pub error: Option<String>,
    cache_dir: PathBuf,
    backend: Arc<dyn CatalogBackend>,
    rx: Option<mpsc::Receiver<Update>>,
}

impl std::fmt::Debug for CatalogState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogState")
            .field("modified", &self.modified)
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
            modified: None,
            error: None,
            cache_dir,
            backend,
            rx: None,
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
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string());
            match result {
                Ok(runtime) => runtime.block_on(async {
                    let before = modified(&dir);
                    match backend.fetch(request, &dir).await {
                        Ok(mut catalog) => {
                            let refresh = catalog.take_refresh();
                            let after = modified(&dir);
                            // force_refresh returns cached success when the fetch fails.
                            let warning = (request == CatalogRequest::ForceRefresh && before.is_some() && before == after)
                                .then(|| "Catalog was not updated; using cached data (fetch failed or refresh busy)".into());
                            let _ = tx.send(Update { result: Ok(Arc::new(catalog)), modified: after, busy: refresh.is_some(), warning });
                            if let Some(refresh) = refresh {
                                let result = match refresh.await {
                                    Ok(result) => result.map(Arc::new).map_err(|e| e.to_string()),
                                    Err(error) => Err(error.to_string()),
                                };
                                let _ = tx.send(Update { result, modified: modified(&dir), busy: false, warning: None });
                            }
                        }
                        Err(error) => { let _ = tx.send(Update { result: Err(error), modified: before, busy: false, warning: None }); }
                    }
                }),
                Err(error) => { let _ = tx.send(Update { result: Err(error), modified: None, busy: false, warning: None }); }
            }
        });
    }

    pub fn poll(&mut self) -> bool {
        let Some(rx) = self.rx.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(update) => {
                match update.result {
                    Ok(catalog) => {
                        self.catalog = Some(catalog);
                        self.modified = update.modified;
                        self.error = update.warning;
                    }
                    Err(error) => self.error = Some(error),
                }
                if update.busy {
                    self.rx = Some(rx);
                }
                true
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
                false
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.error = Some("Catalog worker stopped without a result".into());
                true
            }
        }
    }
}

fn modified(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir.join("models-dev.json"))
        .ok()?
        .modified()
        .ok()
}
