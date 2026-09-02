//! keyTriggers セクションのレンダラ (issue #49 / AC9)。
//!
//! ロールとセクションの対応表 (トリガー一覧) を決定論的にレンダリングする。
//! 入力の順序に依存しないよう、名前のバイト列順で安定ソートし、同名は
//! 先勝ちで重複排除する。

/// keyTriggers の 1 エントリ。セクション名とその説明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerSource {
    /// セクション名 (ソート・重複排除のキー)。
    pub name: String,
    /// セクションの説明。
    pub description: String,
}

/// keyTriggers ブロックの固定ヘッダ。
const KEY_TRIGGERS_HEADER: &str = "### keyTriggers\n\n";

/// keyTriggers ブロックの固定フッタ。
const KEY_TRIGGERS_FOOTER: &str =
    "\n\n該当するトリガーが存在する場合のみ、対応するセクションを参照すること。";

/// 空集合のときの固定本文。
const KEY_TRIGGERS_EMPTY_BODY: &str = "(該当なし)";

/// keyTriggers ブロックをレンダリングする純粋関数。
///
/// - 名前のバイト列順で安定ソートする。
/// - 同名のエントリは先に現れた description を優先して重複排除する。
/// - ヘッダ・フッタは固定で、空集合でも有効なブロックを返す。
pub fn render_key_triggers(sources: &[TriggerSource]) -> String {
    let mut sorted: Vec<&TriggerSource> = sources.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    sorted.dedup_by(|a, b| a.name == b.name);

    let mut rendered = String::from(KEY_TRIGGERS_HEADER);
    if sorted.is_empty() {
        rendered.push_str(KEY_TRIGGERS_EMPTY_BODY);
    } else {
        for source in &sorted {
            rendered.push_str("- ");
            rendered.push_str(&source.name);
            rendered.push_str(": ");
            rendered.push_str(&source.description);
            rendered.push('\n');
        }
    }
    rendered.push_str(KEY_TRIGGERS_FOOTER);
    rendered
}

/// ADR 0002 の 4 ロールからデフォルトの keyTriggers を構築する。
///
/// 各エントリはロール名とその [`agents::RoleCapabilities`] の要約からなる。
pub fn default_role_triggers() -> Vec<TriggerSource> {
    const ROLES: [agents::Role; 4] = [
        agents::Role::Orchestrator,
        agents::Role::Explorer,
        agents::Role::Worker,
        agents::Role::Reviewer,
    ];
    ROLES
        .iter()
        .map(|role| TriggerSource {
            name: role.name().to_owned(),
            description: capability_summary(&role.capabilities()),
        })
        .collect()
}

/// RoleCapabilities を keyTriggers の説明文 1 行に要約する。
fn capability_summary(caps: &agents::RoleCapabilities) -> String {
    let network = match caps.network {
        agents::NetworkAccess::Denied => "拒否",
        agents::NetworkAccess::OptIn => "明示的オプトイン時のみ許可",
        agents::NetworkAccess::Allowed => "許可",
    };
    let can_delegate = if caps.can_delegate { "可" } else { "不可" };
    let tools = caps
        .allowed_tools
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    format!("許可ツール: {tools} / ネットワーク: {network} / 委譲: {can_delegate}")
}

/// assembly 時点で利用可能な agent の metadata。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableAgent {
    pub name: String,
    pub description: String,
}

/// assembly 時点で利用可能な skill の metadata。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkill {
    pub name: String,
    pub description: String,
}

/// available agents / skills metadata から keyTriggers 用 TriggerSource を生成する。
///
/// 両 source を横断して name で重複排除し (同一 name が agents と skills に現れた場合は
/// agent 側を優先)、name の昇順で安定順序化する。空集合でも有効 (空 Vec を返す)。
pub fn triggers_from_availability(
    agents: &[AvailableAgent],
    skills: &[AvailableSkill],
) -> Vec<TriggerSource> {
    let mut sources: Vec<TriggerSource> = agents
        .iter()
        .map(|entry| TriggerSource {
            name: entry.name.clone(),
            description: entry.description.clone(),
        })
        .chain(skills.iter().map(|entry| TriggerSource {
            name: entry.name.clone(),
            description: entry.description.clone(),
        }))
        .collect();
    sources.sort_by(|a, b| a.name.cmp(&b.name));
    sources.dedup_by(|a, b| a.name == b.name);
    sources
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, description: &str) -> TriggerSource {
        TriggerSource {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    // Given: 名前順がバラバラのトリガー集合
    // When: render_key_triggers する
    // Then: 名前のバイト列順にソートされた箇条書きになる
    #[test]
    fn key_triggers_render_sorted_by_name() {
        let rendered = render_key_triggers(&[
            source("Worker", "worker-desc"),
            source("Orchestrator", "orchestrator-desc"),
            source("Explorer", "explorer-desc"),
        ]);

        let orchestrator = rendered
            .find("- Orchestrator:")
            .expect("Orchestrator 行があるはずです");
        let explorer = rendered
            .find("- Explorer:")
            .expect("Explorer 行があるはずです");
        let worker = rendered.find("- Worker:").expect("Worker 行があるはずです");
        assert!(explorer < orchestrator && orchestrator < worker);
    }

    // Given: 同名のトリガーが重複して含まれる
    // When: render_key_triggers する
    // Then: 先頭の description が優先され 1 行にまとめられる
    #[test]
    fn key_triggers_dedup_duplicate_names() {
        let rendered = render_key_triggers(&[
            source("Worker", "first-description"),
            source("Worker", "second-description"),
        ]);

        let count = rendered
            .lines()
            .filter(|l| l.starts_with("- Worker:"))
            .count();
        assert_eq!(count, 1);
        assert!(rendered.contains("- Worker: first-description"));
        assert!(!rendered.contains("second-description"));
    }

    // Given: 空のトリガー集合
    // When: render_key_triggers する
    // Then: ヘッダと (該当なし) 行を含む有効なブロックになる
    #[test]
    fn key_triggers_empty_set_renders_valid_block() {
        let rendered = render_key_triggers(&[]);

        assert!(rendered.starts_with("### keyTriggers"));
        assert!(rendered.contains("(該当なし)"));
        assert!(!rendered.lines().any(|l| l.starts_with("- ")));
    }

    // Given: 同一のトリガー集合
    // When: render_key_triggers を 2 回呼ぶ
    // Then: 出力はバイト単位で同一になる
    #[test]
    fn key_triggers_render_is_deterministic_for_same_input() {
        let sources = vec![
            source("Orchestrator", "orchestrator-desc"),
            source("Worker", "worker-desc"),
        ];

        assert_eq!(render_key_triggers(&sources), render_key_triggers(&sources));
    }

    // Given: ADR 0002 の 4 ロール
    // When: default_role_triggers を構築する
    // Then: 4 ロール分のエントリがケイパビリティ要約付きで固定順に並ぶ
    #[test]
    fn default_role_triggers_lists_all_four_roles_with_capability_summary() {
        let triggers = default_role_triggers();

        let names: Vec<&str> = triggers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Orchestrator", "Explorer", "Worker", "Reviewer"]
        );
        for trigger in &triggers {
            assert!(
                trigger.description.contains("ネットワーク")
                    && trigger.description.contains("委譲"),
                "ケイパビリティ要約が含まれるはずです: {}",
                trigger.description
            );
        }
    }

    fn agent(name: &str, description: &str) -> AvailableAgent {
        AvailableAgent {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    fn skill(name: &str, description: &str) -> AvailableSkill {
        AvailableSkill {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    // Given: 名前順がバラバラの agents と skills
    // When: triggers_from_availability する
    // Then: name の昇順にマージされ、description も対応付けられる
    #[test]
    fn triggers_from_availability_merges_agents_and_skills_sorted() {
        let agents = [agent("zeta", "zeta-desc"), agent("alpha", "alpha-desc")];
        let skills = [skill("beta", "beta-desc")];

        let triggers = triggers_from_availability(&agents, &skills);

        let names: Vec<&str> = triggers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "zeta"]);
        let descriptions: Vec<&str> = triggers.iter().map(|t| t.description.as_str()).collect();
        assert_eq!(descriptions, vec!["alpha-desc", "beta-desc", "zeta-desc"]);
    }

    // Given: 同一 name が agents と skills の両方に現れる
    // When: triggers_from_availability する
    // Then: 1 エントリにまとめられ、description は agent 側が優先される
    #[test]
    fn triggers_from_availability_dedups_across_agents_and_skills() {
        let agents = [agent("quick", "agent-desc")];
        let skills = [skill("quick", "skill-desc"), skill("other", "other-desc")];

        let triggers = triggers_from_availability(&agents, &skills);

        let quick: Vec<&TriggerSource> = triggers.iter().filter(|t| t.name == "quick").collect();
        assert_eq!(quick.len(), 1);
        assert_eq!(quick[0].description, "agent-desc");
        assert!(triggers.iter().any(|t| t.name == "other"));
    }

    // Given: 空の agents と skills
    // When: triggers_from_availability して既存レンダラに渡す
    // Then: 空 Vec が返り、レンダラは既存の (該当なし) 空状態ブロックを出す
    #[test]
    fn triggers_from_availability_empty_inputs_yield_empty_and_renderer_stays_valid() {
        let triggers = triggers_from_availability(&[], &[]);

        assert!(triggers.is_empty());

        let rendered = render_key_triggers(&triggers);
        assert!(rendered.starts_with("### keyTriggers"));
        assert!(rendered.contains("(該当なし)"));
        assert!(!rendered.lines().any(|l| l.starts_with("- ")));
    }

    // Given: 同一要素を異なる入力順で与える
    // When: triggers_from_availability を繰り返し・順序を変えて呼ぶ
    // Then: 呼び出しごとに同一出力で、入力順が違っても構造的に同一になる
    #[test]
    fn triggers_from_availability_is_deterministic() {
        let first = triggers_from_availability(
            &[agent("b", "b-desc"), agent("a", "a-desc")],
            &[skill("c", "c-desc"), skill("d", "d-desc")],
        );
        let repeated = triggers_from_availability(
            &[agent("b", "b-desc"), agent("a", "a-desc")],
            &[skill("c", "c-desc"), skill("d", "d-desc")],
        );
        assert_eq!(first, repeated);

        let scrambled = triggers_from_availability(
            &[agent("a", "a-desc"), agent("b", "b-desc")],
            &[skill("d", "d-desc"), skill("c", "c-desc")],
        );
        assert_eq!(first, scrambled);
    }
}
