//! Oracle → Advisor compatibility at the existing pre-merge configuration seam.

use std::collections::{BTreeMap, BTreeSet};

use crate::merge::deep_merge;

/// `None` addresses the root; `Some(name)` addresses one role profile. Keeping the
/// name separate from a dotted path also supports profile names containing dots.
pub(super) type Scope = Option<String>;

#[derive(Clone, Copy, Default)]
struct Origin {
    implicit_oracle_chain: bool,
    preferred_route: Option<ImplicitRoute>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImplicitRoute {
    Oracle,
    Advisor,
}

/// Migration-only state. Never serialized into user TOML, even temporarily.
/// A models replacement drops its origin regardless of whether the new array
/// happens to equal the generated chain.
#[derive(Default)]
struct Provenance(BTreeMap<Scope, Origin>);

pub(crate) struct Layer {
    pub(crate) value: toml::Value,
    provenance: Provenance,
}

impl Layer {
    /// Current-format env/CLI/GUI overrides need alias normalization, not v2 migration.
    pub(crate) fn current(value: toml::Value) -> Self {
        Self::migrated(value, BTreeSet::new())
    }

    pub(super) fn migrated(mut value: toml::Value, implicit_oracle: BTreeSet<Scope>) -> Self {
        let mut provenance = Provenance::default();
        for_each_scope(&mut value, |name, scope| {
            let generated = implicit_oracle.contains(&name);
            let preferred_route = if scope
                .get("routing")
                .and_then(|routing| routing.get("routes"))
                .is_some_and(|routes| routes.get("advisor").is_some())
            {
                Some(ImplicitRoute::Advisor)
            } else if generated {
                Some(ImplicitRoute::Oracle)
            } else {
                None
            };
            // Normalization deep-merges old and new bindings in the same file.
            // New models (or a non-table binding) replace the synthesized array.
            let implicit_oracle_chain =
                generated && !replaces_path(scope.get("agents"), &["roles", "advisor", "models"]);
            normalize_scope(scope);
            provenance.0.insert(
                name,
                Origin {
                    implicit_oracle_chain,
                    preferred_route,
                },
            );
        });
        Self { value, provenance }
    }

    /// Merge TOML and its side-table together using the same replacement boundaries.
    pub(crate) fn merge(&mut self, upper: Self) {
        for (scope, origin) in &mut self.provenance.0 {
            let mut path = scope_path(scope);
            path.extend(["agents", "roles", "advisor", "models"]);
            if replaces_path(Some(&upper.value), &path) {
                origin.implicit_oracle_chain = false;
            }
            if replaces_scope(&upper.value, &scope_path(scope)) {
                origin.preferred_route = None;
            }
        }
        for (scope, upper_origin) in upper.provenance.0 {
            let origin = self.provenance.0.entry(scope).or_default();
            origin.implicit_oracle_chain |= upper_origin.implicit_oracle_chain;
            if upper_origin.preferred_route.is_some() {
                origin.preferred_route = upper_origin.preferred_route;
            }
        }
        let lower = std::mem::replace(&mut self.value, toml::Value::Table(Default::default()));
        self.value = deep_merge(lower, upper.value);
    }

    /// Resolve deferred Advisor chains after all layers (or a standalone save).
    /// Legacy Oracle effort migration keeps its per-file, pre-rename precedence.
    pub(crate) fn finish(mut self) -> toml::Value {
        for_each_scope(&mut self.value, |name, scope| {
            let origin = self.provenance.0.get(&name).copied().unwrap_or_default();
            preserve_implicit_route(scope, origin);
        });
        self.value
    }
}

fn scope_path(scope: &Scope) -> Vec<&str> {
    scope
        .as_deref()
        .map_or_else(Vec::new, |name| vec!["role_profiles", name])
}

/// Missing fields merge; a present leaf or non-table ancestor replaces the old value.
fn replaces_path(value: Option<&toml::Value>, path: &[&str]) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Some((key, rest)) = path.split_first() else {
        return true;
    };
    match value.as_table() {
        Some(table) => replaces_path(table.get(*key), rest),
        None => true,
    }
}

fn replaces_scope(value: &toml::Value, path: &[&str]) -> bool {
    let Some(table) = value.as_table() else {
        return true;
    };
    path.split_first().is_some_and(|(key, rest)| {
        table
            .get(*key)
            .is_some_and(|value| replaces_scope(value, rest))
    })
}

fn for_each_scope(value: &mut toml::Value, mut visit: impl FnMut(Scope, &mut toml::value::Table)) {
    let Some(root) = value.as_table_mut() else {
        return;
    };
    visit(None, root);
    if let Some(profiles) = root
        .get_mut("role_profiles")
        .and_then(toml::Value::as_table_mut)
    {
        for (name, profile) in profiles {
            if let Some(profile) = profile.as_table_mut() {
                visit(Some(name.clone()), profile);
            }
        }
    }
}

fn normalize_scope(scope: &mut toml::value::Table) {
    let Some(roles) = scope
        .get_mut("agents")
        .and_then(|agents| agents.get_mut("roles"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    if let Some(old) = roles.remove("oracle") {
        let binding = match roles.remove("advisor") {
            None => old,
            Some(new) => {
                tracing::warn!(
                    "both oracle and advisor bindings found; advisor fields take precedence"
                );
                deep_merge(old, new)
            }
        };
        roles.insert("advisor".into(), binding);
    }
}

fn preserve_implicit_route(scope: &mut toml::value::Table, origin: Origin) {
    let Some(routes) = scope
        .get_mut("routing")
        .and_then(|routing| routing.get_mut("routes"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    let advisor_route = routes.contains_key("advisor");
    let mut effort = None;
    if let Some(candidates) = routes
        .get_mut("advisor")
        .and_then(toml::Value::as_array_mut)
    {
        for (index, candidate) in candidates.iter_mut().enumerate() {
            let removed = candidate
                .as_table_mut()
                .and_then(|candidate| candidate.remove("reasoning_effort"));
            if index == 0 {
                effort = removed.and_then(|effort| effort.as_str().map(str::to_owned));
            }
        }
    }
    if origin.implicit_oracle_chain && origin.preferred_route != Some(ImplicitRoute::Advisor) {
        return;
    }
    let route = if advisor_route {
        if effort.is_none() && !origin.implicit_oracle_chain {
            return;
        }
        "advisor"
    } else if routes.contains_key("oracle") {
        "oracle"
    } else {
        return;
    };
    let mut node = scope;
    for key in ["agents", "roles", "advisor"] {
        let value = node
            .entry(key)
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let Some(table) = value.as_table_mut() else {
            return;
        };
        node = table;
    }
    if (origin.implicit_oracle_chain
        || node
            .get("models")
            .is_none_or(|models| models.as_array().is_some_and(Vec::is_empty)))
        && !node.contains_key("logical_model")
    {
        let mut entry =
            toml::map::Map::from_iter([("route".into(), toml::Value::String(route.into()))]);
        if let Some(effort) = effort {
            entry.insert("reasoning_effort".into(), toml::Value::String(effort));
        }
        node.insert(
            "models".into(),
            toml::Value::Array(vec![toml::Value::Table(entry)]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Layer;

    fn migrated(source: &str) -> Layer {
        crate::migrate::run_layer(toml::from_str(source).unwrap()).unwrap()
    }

    #[test]
    fn intermediate_toml_has_no_generated_metadata_and_preserves_user_fields() {
        for route in ["oracle", "advisor"] {
            let source = format!(
                "[routing.routes]\n{route} = [{{ profile = 'p', reasoning_effort = 'low' }}]\n\
                 [role_profiles.'with.dot'.routing.routes]\n{route} = [{{ profile = 'p', reasoning_effort = 'low' }}]\n"
            );
            let layer = migrated(&source);
            assert!(!toml::to_string(&layer.value).unwrap().contains("__evorch_"));
            let source = format!("__evorch_implicit_advisor_route = false\n{source}");
            let layer = migrated(&source);
            assert_eq!(
                layer.value["__evorch_implicit_advisor_route"].as_bool(),
                Some(false)
            );
            assert_eq!(
                layer.finish()["__evorch_implicit_advisor_route"].as_bool(),
                Some(false)
            );
        }
    }

    #[test]
    fn partial_bindings_keep_origin_and_scopes_do_not_interfere() {
        let mut lower = migrated(
            "[routing.routes]\noracle = [{ profile = 'p', reasoning_effort = 'low' }]\n\
             [role_profiles.'with.dot'.routing.routes]\noracle = [{ profile = 'p', reasoning_effort = 'low' }]\n",
        );
        lower.merge(Layer::current(toml::from_str(
            "[agents.roles.advisor]\npreset = 'role-advisor'\n\
             [routing.routes]\nadvisor = [{ profile = 'q', reasoning_effort = 'high' }]\n\
             [role_profiles.'with.dot'.agents.roles.oracle]\nmodels = [{ route = 'oracle', reasoning_effort = 'low' }]\n\
             [role_profiles.'with.dot'.routing.routes]\nadvisor = [{ profile = 'q', reasoning_effort = 'high' }]\n",
        ).unwrap()));
        let value = lower.finish();
        assert_eq!(
            value["agents"]["roles"]["advisor"]["models"][0]["route"].as_str(),
            Some("advisor")
        );
        assert_eq!(
            value["role_profiles"]["with.dot"]["agents"]["roles"]["advisor"]["models"][0]["route"]
                .as_str(),
            Some("oracle")
        );
    }

    #[test]
    fn replacing_ancestors_discards_generated_origin() {
        for replacement in [
            "agents = false",
            "[agents]\nroles = false",
            "[agents.roles]\nadvisor = false",
        ] {
            let mut lower = migrated(
                "[routing.routes]\noracle = [{ profile = 'p', reasoning_effort = 'low' }]\n",
            );
            lower.merge(Layer::current(toml::from_str(replacement).unwrap()));
            assert!(!lower.provenance.0[&None].implicit_oracle_chain);
            lower.merge(Layer::current(
                toml::from_str(
                    "[agents.roles.advisor]\nmodels = [{ route = 'custom' }]\n\
                 [routing.routes]\nadvisor = [{ profile = 'p', reasoning_effort = 'high' }]\n",
                )
                .unwrap(),
            ));
            let value = lower.finish();
            assert_eq!(
                value["agents"]["roles"]["advisor"]["models"][0]["route"].as_str(),
                Some("custom")
            );
        }
    }
}
