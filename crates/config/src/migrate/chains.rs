//! v2 の `logical_model` と route 候補の `reasoning_effort` を v3 のチェーンへ移す (ADR 0030)。
//!
//! 1 ファイル分の値だけを見る。route と agents が別の層にあると推論強度を引けないため、
//! その場合は推論強度なしで移して警告を返す。

use std::collections::{BTreeMap, BTreeSet};

use super::roles::Scope;

#[derive(Default)]
pub(super) struct ChainMigration {
    pub(super) warnings: Vec<String>,
    pub(super) implicit_oracle: BTreeSet<Scope>,
}

/// 移行の対象になるロールの設定上のアドレス (`agents.` からの相対)。
const ROLE_ADDRESSES: [&str; 9] = [
    "orchestrator",
    "explorer",
    "worker",
    "reviewer",
    "roles.web_researcher",
    "roles.planner",
    "roles.advisor",
    "roles.oracle", // Convert legacy logical_model before renaming the binding.
    "roles.multimodal_looker",
];

/// route 名 → 先頭候補の推論強度 (候補が持たなければ `None`)。
type RouteEfforts = BTreeMap<String, Option<String>>;

/// 旧形式のフィールドをチェーンへ移し、警告と生成した Oracle chain の位置を返す。
///
/// 旧形式のキーがなければ何もしないため、version に関わらず適用できる。
pub(super) fn move_to_chains(value: &mut toml::Value) -> ChainMigration {
    let mut result = ChainMigration::default();
    let Some(root) = value.as_table_mut() else {
        return result;
    };
    if migrate_scope(root, "", &mut result.warnings) {
        result.implicit_oracle.insert(None);
    }
    if let Some(profiles) = root
        .get_mut("role_profiles")
        .and_then(toml::Value::as_table_mut)
    {
        for (name, profile) in profiles.iter_mut() {
            if let Some(profile) = profile.as_table_mut()
                && migrate_scope(
                    profile,
                    &format!("role_profiles.{name}."),
                    &mut result.warnings,
                )
            {
                result.implicit_oracle.insert(Some(name.clone()));
            }
        }
    }
    result
}

/// `agents` と `routing` を同じ階層に持つ 1 スコープ (トップレベルかロールプロファイル) を移す。
fn migrate_scope(scope: &mut toml::value::Table, prefix: &str, warnings: &mut Vec<String>) -> bool {
    let efforts = take_route_efforts(scope, prefix, warnings);
    let Some(agents) = scope.get_mut("agents").and_then(toml::Value::as_table_mut) else {
        return add_implicit_chains(scope, prefix, &efforts, warnings);
    };
    for address in ROLE_ADDRESSES {
        let path = format!("{prefix}agents.{address}");
        let Some(binding) = binding_mut(agents, address) else {
            continue;
        };
        convert_binding(binding, &path, &efforts, warnings);
        if let Some(categories) = binding
            .get_mut("categories")
            .and_then(toml::Value::as_table_mut)
        {
            for (category, binding) in categories.iter_mut() {
                if let Some(binding) = binding.as_table_mut() {
                    convert_binding(
                        binding,
                        &format!("{path}.categories.{category}"),
                        &efforts,
                        warnings,
                    );
                }
            }
        }
    }
    add_implicit_chains(scope, prefix, &efforts, warnings)
}

/// route 候補から `reasoning_effort` を取り除き、route ごとの推論強度を返す。
/// `routing.routes.advisor` の effort だけは全層マージまで保持する。新名 route からの
/// 暗黙 chain が他層の明示 chain を上書きしないための改称固有の例外である。
/// 単独ファイルの保存では、呼び出し側がマージ相当の最終化まで行う。
fn take_route_efforts(
    scope: &mut toml::value::Table,
    prefix: &str,
    warnings: &mut Vec<String>,
) -> RouteEfforts {
    let mut efforts = RouteEfforts::new();
    let Some(routes) = scope
        .get_mut("routing")
        .and_then(|routing| routing.get_mut("routes"))
        .and_then(toml::Value::as_table_mut)
    else {
        return efforts;
    };
    for (name, candidates) in routes.iter_mut() {
        let Some(candidates) = candidates.as_array_mut() else {
            continue;
        };
        let taken: Vec<Option<String>> = candidates
            .iter_mut()
            .map(|candidate| {
                candidate.as_table_mut().and_then(|candidate| {
                    // Keep the new implicit route's effort with its layer until
                    // the merged configuration can decide whether it is used.
                    let effort = if name == "advisor" {
                        candidate.get("reasoning_effort").cloned()
                    } else {
                        candidate.remove("reasoning_effort")
                    };
                    effort.and_then(|effort| effort.as_str().map(str::to_owned))
                })
            })
            .collect();
        let first = taken.first().cloned().flatten();
        if taken.iter().any(|effort| *effort != first) {
            warnings.push(format!(
                "{prefix}routing.routes.{name}: candidates had different reasoning_effort values; \
                 moved the first candidate's ({}) to the roles that use this route",
                first.as_deref().unwrap_or("provider default")
            ));
        }
        efforts.insert(name.clone(), first);
    }
    efforts
}

/// `logical_model = "x"` を `models = [{ route = "x", reasoning_effort = ... }]` に書き換える。
fn convert_binding(
    binding: &mut toml::value::Table,
    path: &str,
    efforts: &RouteEfforts,
    warnings: &mut Vec<String>,
) {
    let Some(logical) = binding.remove("logical_model") else {
        return;
    };
    let Some(route) = logical.as_str() else {
        warnings.push(format!("{path}.logical_model: ignored a non-string value"));
        return;
    };
    if binding.contains_key("models") {
        warnings.push(format!(
            "{path}.logical_model: ignored because models is also set"
        ));
        return;
    }
    let effort = match efforts.get(route) {
        Some(effort) => effort.clone(),
        None => {
            warnings.push(format!(
                "{path}.logical_model: route `{route}` is not in the same file, so no \
                 reasoning_effort was moved; set it on {path}.models if needed"
            ));
            None
        }
    };
    binding.insert("models".into(), chain(route, effort));
}

/// ロール名と同名の route に推論強度があり、同じファイルでそのロールを割り当てていない場合、
/// 推論強度を失わないよう暗黙の割り当てを明示的なチェーンにする。
fn add_implicit_chains(
    scope: &mut toml::value::Table,
    prefix: &str,
    efforts: &RouteEfforts,
    warnings: &mut Vec<String>,
) -> bool {
    let mut implicit_oracle = false;
    for address in ROLE_ADDRESSES {
        let role = address.strip_prefix("roles.").unwrap_or(address);
        if role == "advisor" || (role == "oracle" && efforts.contains_key("advisor")) {
            // Advisor did not name a role before the rename. Defer its implicit
            // chain until after merging so it cannot replace an explicit binding.
            continue;
        }
        let Some(Some(effort)) = efforts.get(role) else {
            continue;
        };
        let agents = scope
            .entry("agents")
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        let Some(agents) = agents.as_table_mut() else {
            return implicit_oracle;
        };
        let binding = match address.split_once('.') {
            Some((group, name)) => agents
                .entry(group)
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
                .as_table_mut()
                .map(|group| {
                    group
                        .entry(name)
                        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
                }),
            None => Some(
                agents
                    .entry(address)
                    .or_insert_with(|| toml::Value::Table(toml::value::Table::new())),
            ),
        };
        let Some(binding) = binding.and_then(toml::Value::as_table_mut) else {
            continue;
        };
        if binding.contains_key("models") {
            continue;
        }
        binding.insert("models".into(), chain(role, Some(effort.clone())));
        implicit_oracle |= role == "oracle";
        warnings.push(format!(
            "{prefix}agents.{address}.models: moved reasoning_effort `{effort}` from \
             {prefix}routing.routes.{role}, which the role used implicitly"
        ));
    }
    implicit_oracle
}

fn binding_mut<'a>(
    agents: &'a mut toml::value::Table,
    address: &str,
) -> Option<&'a mut toml::value::Table> {
    match address.split_once('.') {
        Some((group, name)) => agents
            .get_mut(group)?
            .as_table_mut()?
            .get_mut(name)?
            .as_table_mut(),
        None => agents.get_mut(address)?.as_table_mut(),
    }
}

fn chain(route: &str, effort: Option<String>) -> toml::Value {
    let mut entry = toml::value::Table::new();
    entry.insert("route".into(), toml::Value::String(route.to_owned()));
    if let Some(effort) = effort {
        entry.insert("reasoning_effort".into(), toml::Value::String(effort));
    }
    toml::Value::Array(vec![toml::Value::Table(entry)])
}

#[cfg(test)]
mod tests {
    use super::move_to_chains;

    fn migrate(text: &str) -> (toml::Value, Vec<String>) {
        let mut value: toml::Value = toml::from_str(text).expect("TOML");
        let warnings = move_to_chains(&mut value).warnings;
        (value, warnings)
    }

    fn models(value: &toml::Value, path: &[&str]) -> toml::Value {
        let mut node = value;
        for key in path {
            node = &node[*key];
        }
        node["models"].clone()
    }

    // Given: route の候補に推論強度があり、ロールとカテゴリが logical_model で参照する設定
    // When: 移行する
    // Then: 推論強度はチェーンの要素へ移り、route 側からは消える。警告はない
    #[test]
    fn moves_candidate_effort_to_referencing_roles_and_categories() {
        let (value, warnings) = migrate(
            r#"
[[routing.routes.sol]]
profile = "codex-work"
reasoning_effort = "high"
[[routing.routes.sol]]
profile = "codex-personal"
reasoning_effort = "high"
[agents.orchestrator]
logical_model = "sol"
[agents.worker.categories.quick]
logical_model = "sol"
"#,
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        let expected: toml::Value =
            toml::from_str("models = [{ route = 'sol', reasoning_effort = 'high' }]").unwrap();
        assert_eq!(
            models(&value, &["agents", "orchestrator"]),
            expected["models"]
        );
        assert_eq!(
            models(&value, &["agents", "worker", "categories", "quick"]),
            expected["models"]
        );
        assert!(
            value["agents"]["orchestrator"]
                .get("logical_model")
                .is_none()
        );
        for candidate in value["routing"]["routes"]["sol"].as_array().unwrap() {
            assert!(candidate.get("reasoning_effort").is_none());
        }
    }

    // Given: 候補ごとに推論強度が異なる route と、同じファイルにない route を参照するロール
    // When: 移行する
    // Then: 先頭の推論強度を採用し、どちらも警告する
    #[test]
    fn warns_on_conflicting_efforts_and_routes_in_other_files() {
        let (value, warnings) = migrate(
            r#"
[[routing.routes.mixed]]
profile = "a"
reasoning_effort = "low"
[[routing.routes.mixed]]
profile = "b"
reasoning_effort = "high"
[agents.explorer]
logical_model = "mixed"
[agents.roles.planner]
logical_model = "elsewhere"
"#,
        );

        assert_eq!(
            value["agents"]["explorer"]["models"][0]["reasoning_effort"].as_str(),
            Some("low")
        );
        assert_eq!(
            value["agents"]["roles"]["planner"]["models"][0].get("reasoning_effort"),
            None
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("routing.routes.mixed"));
        assert!(warnings[1].contains("agents.roles.planner.logical_model"));
    }

    // Given: ロール名と同名の route に推論強度があり、そのロールを明示的に割り当てていない設定
    // When: 移行する
    // Then: 推論強度を持つ明示的なチェーンが追加され、警告される
    #[test]
    fn keeps_effort_of_implicitly_used_role_route() {
        let (value, warnings) = migrate(
            r#"
[[routing.routes.worker]]
profile = "main"
reasoning_effort = "xhigh"
[[routing.routes.unused]]
profile = "main"
reasoning_effort = "low"
"#,
        );

        assert_eq!(
            value["agents"]["worker"]["models"][0]["route"].as_str(),
            Some("worker")
        );
        assert_eq!(
            value["agents"]["worker"]["models"][0]["reasoning_effort"].as_str(),
            Some("xhigh")
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    // Given: ロールプロファイル内の route と agents
    // When: 移行する
    // Then: プロファイル内の route から推論強度を引く
    #[test]
    fn migrates_role_profiles_with_their_own_routes() {
        let (value, warnings) = migrate(
            r#"
[[role_profiles.cheap.routing.routes.fast]]
profile = "kimi"
reasoning_effort = "medium"
[role_profiles.cheap.agents.reviewer]
logical_model = "fast"
"#,
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            value["role_profiles"]["cheap"]["agents"]["reviewer"]["models"][0]["reasoning_effort"]
                .as_str(),
            Some("medium")
        );
    }

    // Given: すでに v3 形式の設定 / When: 移行する / Then: 何も変わらない
    #[test]
    fn leaves_v3_config_untouched() {
        let text = r#"
[[routing.routes.sol]]
profile = "codex-work"
[agents.orchestrator]
models = [{ route = "sol", reasoning_effort = "high" }]
"#;
        let (value, warnings) = migrate(text);
        assert!(warnings.is_empty());
        assert_eq!(value, toml::from_str::<toml::Value>(text).unwrap());
    }
}
