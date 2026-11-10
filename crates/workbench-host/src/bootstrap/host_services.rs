//! MCP and metadata lifetimes. Diagnostics/usage use the existing owned bridge,
//! not a second subscriber. No constructor discovers or connects to MCP servers.
use super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use recorder::pricing::{SharedUsagePricing, UsagePricing};
use runtime::mcp::{McpCredentialBackend, McpHostServices, SecretUnavailableReason};
use std::{future::Future, path::Path, sync::Arc};

type Refresh = tokio::task::JoinHandle<Result<catalog::ModelCatalog, catalog::CatalogError>>;

pub(super) async fn mcp(directory: &Path) -> Result<McpHostServices, BootstrapError> {
    // MCP deliberately retains its keychain-only contract; provider file fallback
    // is not permission to persist MCP secrets in plaintext.
    let backend = tokio::task::spawn_blocking(|| match sandbox::credential::open_keychain_only() {
        Ok(store) => McpCredentialBackend::Available(store),
        Err(sandbox::CredentialError::KeychainLocked) => {
            McpCredentialBackend::Unavailable(SecretUnavailableReason::Locked)
        }
        Err(sandbox::CredentialError::KeychainCancelled) => {
            McpCredentialBackend::Unavailable(SecretUnavailableReason::Denied)
        }
        Err(_) => McpCredentialBackend::Unavailable(SecretUnavailableReason::Unavailable),
    })
    .await?;
    Ok(McpHostServices::at_config_dir(
        backend,
        directory.to_path_buf(),
    ))
}

fn diagnostic(bus: &EventBus, code: &str, detail: &str, severity: DiagnosticSeverity) {
    bus.emit(Event::new(DiagnosticEvent {
        source: "catalog".into(),
        severity,
        code: code.into(),
        detail: detail.into(),
        run_id: None,
        thread_id: None,
        call_id: None,
    }));
}

async fn catalog_task<Load, Apply>(
    load: Load,
    apply: Apply,
    bus: &EventBus,
    stop: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), BootstrapError>
where
    Load: Future<Output = Result<Option<Refresh>, catalog::CatalogError>>,
    Apply: FnOnce(catalog::ModelCatalog),
{
    // Dropping load or aborting refresh cannot cancel an already-started
    // spawn_blocking cache operation. Keep both joins in the shutdown lifetime.
    match load.await {
        Ok(Some(refresh)) => match refresh.await {
            Ok(Ok(catalog)) => apply(catalog),
            _ => diagnostic(
                bus,
                "catalog.unavailable",
                "Model metadata refresh unavailable",
                DiagnosticSeverity::Warning,
            ),
        },
        Ok(None) => {}
        Err(_) => diagnostic(
            bus,
            "catalog.unavailable",
            "Model metadata unavailable; configured prices retained",
            DiagnosticSeverity::Warning,
        ),
    }
    let _ = stop.await;
    Ok(())
}

pub(super) fn start(
    services: &mut BackgroundServices,
    options: &BootstrapOptions,
    config: &config::Config,
    bus: Arc<EventBus>,
    pricing: SharedUsagePricing,
    mcp: McpHostServices,
) {
    services.start("mcp", |stop| async move {
        let _ = stop.await;
        mcp.shutdown()
            .await
            .map_err(|kind| format!("MCP drain: {kind}").into())
    });
    let directory = options.config_dir.join("catalog");
    if config.providers.is_empty() && !directory.join("models-dev.json").exists() {
        return; // Unconfigured health mode must not require network access.
    }
    let providers = config.providers.clone();
    services.start("catalog", |stop| async move {
        let refreshed_providers = providers.clone();
        let refreshed_pricing = pricing.clone();
        let load = async {
            let mut catalog = catalog::ModelCatalog::load_or_refresh(&directory).await?;
            let refresh = catalog.take_refresh();
            *pricing
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                UsagePricing::new(providers, Some(Arc::new(catalog)));
            diagnostic(
                &bus,
                "catalog.loaded",
                "Model metadata available",
                DiagnosticSeverity::Info,
            );
            Ok(refresh)
        };
        catalog_task(
            load,
            move |catalog| {
                *refreshed_pricing
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    UsagePricing::new(refreshed_providers, Some(Arc::new(catalog)));
            },
            &bus,
            stop,
        )
        .await
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, task::Poll};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn catalog_stop_joins_blocking_load_io() {
        assert_catalog_stop_joins_blocking_io(false).await;
    }

    #[tokio::test]
    async fn catalog_stop_joins_blocking_refresh_io() {
        assert_catalog_stop_joins_blocking_io(true).await;
    }

    async fn assert_catalog_stop_joins_blocking_io(refreshing: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache-io");
        let written = path.clone();
        let (started, opened) = oneshot::channel();
        let (release, may_finish) = oneshot::channel();
        let (finished, completed) = oneshot::channel();
        let io = tokio::task::spawn_blocking(
            move || -> Result<catalog::ModelCatalog, catalog::CatalogError> {
                let mut file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(written)?;
                file.lock()?;
                started.send(()).unwrap();
                may_finish.blocking_recv().unwrap();
                file.write_all(b"finished")?;
                file.sync_all()?;
                drop(file);
                finished.send(()).unwrap();
                Err(std::io::Error::other("fixture catalog result").into())
            },
        );
        let (probe, checkpoint) = oneshot::channel();
        let (probed, checked) = oneshot::channel();
        let load = async move {
            if refreshing {
                // Like the real refresh, this is an async task awaiting blocking
                // I/O, not a directly abort-resistant spawn_blocking handle.
                Ok(Some(tokio::spawn(async move {
                    let _ = checkpoint.await;
                    let _ = probed.send(());
                    io.await.unwrap()
                })))
            } else {
                io.await.unwrap().map(|_| None)
            }
        };
        let (stop, stopped) = oneshot::channel();
        let bus = EventBus::new(16);
        let mut task = Box::pin(catalog_task(
            load,
            |_| unreachable!("fixture returns a classified error"),
            &bus,
            stopped,
        ));
        let initially_pending = std::future::poll_fn(|cx| Poll::Ready(task.as_mut().poll(cx)))
            .await
            .is_pending();
        opened.await.unwrap();
        let _ = stop.send(());
        let stop_pending = std::future::poll_fn(|cx| Poll::Ready(task.as_mut().poll(cx)))
            .await
            .is_pending();
        if refreshing {
            // Drive the async wrapper through its checkpoint (or its abort)
            // before repolling: an abort itself may initially return Pending.
            let _ = probe.send(());
            let _ = checked.await;
        }
        let draining = stop_pending
            && std::future::poll_fn(|cx| Poll::Ready(task.as_mut().poll(cx)))
                .await
                .is_pending();
        // Release I/O even if a regression completed shutdown early, so a
        // failing assertion cannot strand a blocking worker at runtime drop.
        let _ = release.send(());
        completed.await.unwrap();
        if draining {
            task.await.unwrap();
        }
        assert!(
            initially_pending && draining,
            "early shutdown; refresh={refreshing}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"finished");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.try_lock().unwrap();
    }
}
