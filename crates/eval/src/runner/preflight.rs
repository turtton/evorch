//! Reject invalid comparisons before starting any replay.

use std::collections::BTreeSet;

use event_bus::{DiagnosticEvent, EventKind};
use serde::{Deserialize, Serialize};

use super::*;

pub(crate) const REPORT_FILE: &str = "preflight.json";

#[derive(Deserialize, Serialize)]
pub(crate) struct Report {
    skipped_fixed_models: bool,
    checks: Vec<Check>,
}

impl Report {
    pub(crate) fn failure(&self) -> Option<String> {
        self.checks.iter().find_map(|check| {
            check
                .error
                .as_ref()
                .map(|error| format!("{}: {error}", check.label))
        })
    }
}

#[derive(Deserialize, Serialize)]
struct Check {
    label: String,
    models: Vec<roles::ModelRef>,
    error: Option<String>,
    diagnostics: Vec<DiagnosticEvent>,
}

pub(super) async fn check(
    context: &Context,
    configurations: &[(&str, Option<&toml::Value>)],
    dir: &Path,
) -> Report {
    let mut report = Report {
        skipped_fixed_models: context.deps.models.is_some(),
        checks: Vec::new(),
    };
    if report.skipped_fixed_models {
        return report;
    }
    // Live project files may have changed since the captured start. Use the same
    // reconstruction as replay, not the source checkout's current configuration.
    let workspace = match workspace::prepare(
        &context.thread.source_toplevel,
        &context.thread.snapshot,
        &dir.join("preflight-workspace"),
        &context.identity,
    ) {
        Ok(workspace) => workspace,
        Err(error) => {
            report.checks.push(Check {
                label: "workspace".into(),
                models: Vec::new(),
                error: Some(error.to_string()),
                diagnostics: Vec::new(),
            });
            return report;
        }
    };
    let configs: Vec<_> = configurations
        .iter()
        .map(|(label, overlay)| {
            (
                *label,
                load_config(
                    context.user_config_dir.as_deref(),
                    Some(&workspace.project_dir),
                    *overlay,
                )
                .map_err(|error| error.to_string()),
            )
        })
        .collect();
    let swapped = match configs.as_slice() {
        [(_, Ok(baseline)), (_, Ok(candidate))] => roles::swapped_roles(baseline, candidate),
        _ => Vec::new(),
    };
    // Check both sides before spending tokens on either side. Include changed
    // child roles (not just the chat root), with every category and fallback.
    let mut addresses: BTreeSet<&str> = swapped.iter().map(|role| role.role.as_str()).collect();
    addresses.insert(roles::address(context.thread.role.name()).expect("runtime role address"));
    for (label, config) in configs {
        report
            .checks
            .push(check_config(context, label, config, &addresses).await);
    }
    report.checks.push(
        check_config(
            context,
            "user_simulator",
            Ok(context.simulator.clone()),
            &BTreeSet::from(["worker"]),
        )
        .await,
    );
    report
}

async fn check_config(
    context: &Context,
    label: &str,
    config: Result<Config, String>,
    addresses: &BTreeSet<&str>,
) -> Check {
    let bus = Arc::new(EventBus::new(BUS_CAPACITY));
    let mut receiver = bus.subscribe();
    let mut check = Check {
        label: label.into(),
        models: Vec::new(),
        error: None,
        diagnostics: Vec::new(),
    };
    let result = async {
        let config = config?;
        // Model summaries omit unresolved routes. Promote the config validator's
        // target-chain warnings to failures before collecting those summaries.
        let prefixes: Vec<_> = addresses
            .iter()
            .map(|role| format!("agents.{role}."))
            .collect();
        if let Some(problem) = config.chain_problems().into_iter().find(|problem| {
            matches!(problem, config::ConfigError::InvalidField { path, .. }
                if prefixes.iter().any(|prefix| path.starts_with(prefix)))
        }) {
            return Err(problem.to_string());
        }
        for role in addresses {
            let models = roles::role_models(&config, role);
            let inherits_run_chain =
                *role == "roles.compactor" && config.agents.roles.compactor.models.is_empty();
            if models.is_empty() && !inherits_run_chain {
                return Err(format!(
                    "no model routes resolve for preflight role `{role}`"
                ));
            }
            check.models.extend(models);
        }
        check
            .models
            .sort_by(|a, b| (&a.profile, &a.model).cmp(&(&b.profile, &b.model)));
        check.models.dedup();
        if check.models.is_empty() {
            return Err("no model routes are configured for the preflight roles".into());
        }
        let model = configured_routed_model(
            &config,
            context.user_config_dir.as_deref(),
            &context.credentials,
            &context.deps.env,
            &bus,
        )?;
        let routes: Vec<_> = check
            .models
            .iter()
            .map(|model| routing::ResolvedRoute {
                profile: model.profile.clone(),
                model_id: model.model.clone(),
            })
            .collect();
        model
            .preflight_routes(&routes)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    check.error = result.err();
    // All producers above are awaited. A bus sentinel, not a timeout, closes the
    // evidence stream even when no diagnostics were produced.
    bus.emit(event_bus::Event::new(
        event_bus::MessageEvent::MessageDelta {
            delta: "preflight complete".into(),
            run_id: None,
        },
    ));
    while let Ok(event) = receiver.recv().await {
        match event.kind {
            EventKind::Diagnostic(event) => check.diagnostics.push(event),
            EventKind::Message(event_bus::MessageEvent::MessageDelta {
                delta,
                run_id: None,
            }) if delta == "preflight complete" => break,
            _ => {}
        }
    }
    check
}
