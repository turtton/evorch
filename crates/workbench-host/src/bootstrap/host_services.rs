//! MCP and metadata lifetimes. Diagnostics/usage use the existing owned bridge,
//! not a second subscriber. No constructor discovers or connects to MCP servers.
use super::{BootstrapError, BootstrapOptions, services::BackgroundServices};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event, EventBus};
use recorder::pricing::{SharedUsagePricing, UsagePricing};
use runtime::mcp::{McpCredentialBackend, McpHostServices, SecretUnavailableReason};
use std::{path::Path, sync::Arc};

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
    services.start("catalog", |mut stop| async move {
        let loaded = tokio::select! {
            biased;
            _ = &mut stop => return Ok(()),
            loaded = catalog::ModelCatalog::load_or_refresh(&directory) => loaded,
        };
        match loaded {
            Ok(mut catalog) => {
                let refresh = catalog.take_refresh();
                *pricing.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
                    UsagePricing::new(providers.clone(), Some(Arc::new(catalog)));
                diagnostic(&bus, "catalog.loaded", "Model metadata available", DiagnosticSeverity::Info);
                if let Some(mut refresh) = refresh {
                    tokio::select! {
                        biased;
                        _ = &mut stop => {
                            refresh.abort();
                            let _ = refresh.await;
                            return Ok(());
                        }
                        result = &mut refresh => match result {
                            Ok(Ok(catalog)) => {
                                *pricing.write().unwrap_or_else(std::sync::PoisonError::into_inner) =
                                    UsagePricing::new(providers, Some(Arc::new(catalog)));
                            }
                            _ => diagnostic(&bus, "catalog.unavailable", "Model metadata refresh unavailable", DiagnosticSeverity::Warning),
                        }
                    }
                }
            }
            Err(_) => diagnostic(&bus, "catalog.unavailable", "Model metadata unavailable; configured prices retained", DiagnosticSeverity::Warning),
        }
        let _ = stop.await;
        Ok(())
    });
}
