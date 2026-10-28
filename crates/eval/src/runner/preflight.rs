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
    bindings: Vec<Binding>,
    models: Vec<roles::ModelRef>,
    error: Option<String>,
    diagnostics: Vec<DiagnosticEvent>,
}

#[derive(Clone, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct Binding {
    role: String,
    category: Option<String>,
}

impl Binding {
    fn base(role: &str) -> Self {
        Self {
            role: role.into(),
            category: None,
        }
    }

    fn key(&self) -> String {
        match &self.category {
            Some(category) => format!("{}.categories.{category}", self.role),
            None => self.role.clone(),
        }
    }

    fn entries(&self, config: &Config) -> Result<Vec<config::ChainEntryConfig>, String> {
        if self.role == "roles.compactor" {
            // An unset compactor inherits the running binding already checked.
            return Ok(config.agents.roles.compactor.models.clone());
        }
        config
            .agents
            .binding_for(
                self.role.strip_prefix("roles.").unwrap_or(&self.role),
                self.category.as_deref(),
            )
            .map(|binding| binding.models)
            .map_err(|error| format!("preflight binding `{}`: {error}", self.key()))
    }
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
                bindings: Vec::new(),
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
    // A role-wide model union can hide a missing implicit base route behind a
    // valid category. Resolve the actual starting binding, and independently
    // check each changed child role's base and categories on both sides.
    let mut bindings = BTreeSet::from([Binding {
        role: roles::address(context.thread.role.name())
            .expect("runtime role address")
            .into(),
        category: replay_category(context.thread.role),
    }]);
    for role in &swapped {
        bindings.insert(Binding::base(&role.role));
        for (_, config) in &configs {
            if let Ok(config) = config {
                let categories = match role.role.as_str() {
                    "worker" => Some(&config.agents.worker.categories),
                    "reviewer" => Some(&config.agents.reviewer.categories),
                    _ => None,
                };
                for category in categories
                    .into_iter()
                    .flat_map(|categories| categories.keys())
                {
                    bindings.insert(Binding {
                        role: role.role.clone(),
                        category: Some(category.clone()),
                    });
                }
            }
        }
    }
    for (label, config) in configs {
        report
            .checks
            .push(check_config(context, label, config, &bindings).await);
    }
    report.checks.push(
        check_config(
            context,
            "user_simulator",
            Ok(context.simulator.clone()),
            &BTreeSet::from([Binding::base("worker")]),
        )
        .await,
    );
    report
}

async fn check_config(
    context: &Context,
    label: &str,
    config: Result<Config, String>,
    bindings: &BTreeSet<Binding>,
) -> Check {
    let bus = Arc::new(EventBus::new(BUS_CAPACITY));
    let mut receiver = bus.subscribe();
    let mut check = Check {
        label: label.into(),
        bindings: bindings.iter().cloned().collect(),
        models: Vec::new(),
        error: None,
        diagnostics: Vec::new(),
    };
    let result = async {
        let config = config?;
        let model = configured_routed_model(
            &config,
            context.user_config_dir.as_deref(),
            &context.credentials,
            &context.deps.env,
            &bus,
        )?;
        for binding in bindings {
            let entries = binding.entries(&config)?;
            if binding.role == "roles.compactor" && entries.is_empty() {
                continue;
            }
            let routes = model
                .preflight_chain(&binding.key(), &entries)
                .map_err(|error| error.to_string())?;
            check.models.extend(routes.into_iter().map(|route| {
                roles::ModelRef {
                    provider_type: config
                        .providers
                        .get(&route.profile)
                        .map(|profile| profile.provider_type),
                    profile: route.profile,
                    model: route.model_id,
                }
            }));
        }
        check
            .models
            .sort_by(|a, b| (&a.profile, &a.model).cmp(&(&b.profile, &b.model)));
        check.models.dedup();
        if check.models.is_empty() {
            return Err("no model routes are configured for the preflight roles".into());
        }
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
