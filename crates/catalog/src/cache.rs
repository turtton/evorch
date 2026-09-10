use crate::{Api, CatalogError, ModelCatalog};
use serde::Deserialize;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::Path;

#[derive(Deserialize)]
struct Cached {
    fetched_at: u64,
    api: Api,
}

pub(crate) async fn read(dir: &Path) -> Result<ModelCatalog, CatalogError> {
    let path = dir.join("models-dev.json");
    tokio::task::spawn_blocking(move || {
        let cached: Cached = serde_json::from_reader(File::open(path)?)?;
        Ok(ModelCatalog {
            api: cached.api,
            fetched_at: cached.fetched_at,
            refresh: None,
        })
    })
    .await?
}

pub(crate) async fn lock(dir: &Path) -> Result<File, CatalogError> {
    let dir = dir.to_owned();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("models-dev.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(CatalogError::RefreshBusy),
            Err(TryLockError::Error(error)) => Err(CatalogError::Io(error)),
        }
    })
    .await?
}

pub(crate) fn write(dir: &Path, fetched_at: u64, api: &[u8]) -> Result<(), CatalogError> {
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    write!(file, "{{\"fetched_at\":{fetched_at},\"api\":")?;
    file.write_all(api)?;
    file.write_all(b"}")?;
    file.as_file().sync_all()?;
    file.persist(dir.join("models-dev.json"))
        .map_err(|error| error.error)?;
    Ok(())
}
