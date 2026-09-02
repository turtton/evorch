//! システムプロンプトの決定論的組立 (issue #49 / AC3, AC7, AC10)。
//!
//! 固定順 (role baseline → family section → category overlay → Intent Gate →
//! appendix) でセクションを連結する。各セクションの末尾余白を除去し、
//! 空でないセクションのみを空行 1 つで連結し、末尾改行を付けない。
//! 同一入力に対してバイト単位で同一の出力を返す。

use agents::Role;

use crate::prompt::intent_gate::render_intent_gate;
use crate::prompt::key_triggers::TriggerSource;

/// 組立への入力。各テキストは既に解決済みのセクション本文である。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptInput<'a> {
    /// 対象ロール (Intent Gate の要否と appendix の解決に使う)。
    pub role: Role,
    /// カテゴリ (存在すれば overlay を挿入する)。
    pub category: Option<&'a str>,
    /// model id から分類されたファミリ。
    pub family: crate::prompt::family::ModelFamily,
    /// ロールの baseline セクション本文。
    pub role_baseline: &'a str,
    /// ファミリの base section 本文。
    pub family_section: &'a str,
    /// カテゴリ overlay 本文 (任意)。
    pub category_overlay: Option<&'a str>,
    /// appendix 本文 (任意)。
    pub appendix: Option<&'a str>,
    /// keyTriggers のソース一覧。
    pub triggers: &'a [TriggerSource],
}

/// システムプロンプトを組立てる純粋関数 (AC3)。
///
/// - 各セクションは末尾の余白を削除してから扱う。
/// - 空になったセクション (空文字列・None) は連結から除外する。
/// - Intent Gate はロールが Orchestrator のときのみ挿入する (AC7)。
/// - セクションは空行 1 つ (`\n\n`) で連結し、末尾に改行を付けない。
pub fn assemble_system_prompt(input: &SystemPromptInput<'_>) -> String {
    let mut sections: Vec<String> = vec![
        input.role_baseline.trim_end().to_owned(),
        input.family_section.trim_end().to_owned(),
    ];
    if let Some(overlay) = input.category_overlay {
        sections.push(overlay.trim_end().to_owned());
    }
    if input.role == Role::Orchestrator {
        sections.push(render_intent_gate(input.triggers).trim_end().to_owned());
    }
    if let Some(appendix) = input.appendix {
        sections.push(appendix.trim_end().to_owned());
    }
    sections.retain(|section| !section.is_empty());
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::ModelFamily;

    const BASELINE: &str = "BASELINE-MARKER あなたは調整役です。";
    const FAMILY: &str = "FAMILY-MARKER 出力規約です。";
    const OVERLAY: &str = "OVERLAY-MARKER カテゴリ上書きです。";
    const APPENDIX: &str = "APPENDIX-MARKER 追加指示です。";
    const GATE_MARK: &str = "## Intent Gate";

    fn input(
        role: Role,
        category: Option<&'static str>,
        overlay: Option<&'static str>,
        appendix: Option<&'static str>,
    ) -> SystemPromptInput<'static> {
        SystemPromptInput {
            role,
            category,
            family: ModelFamily::Claude,
            role_baseline: BASELINE,
            family_section: FAMILY,
            category_overlay: overlay,
            appendix,
            triggers: &[],
        }
    }

    // Given: Orchestrator と全レイヤーが存在する入力
    // When: assemble_system_prompt する
    // Then: baseline → family → overlay → gate → appendix の順に連結される
    #[test]
    fn assembly_orders_sections_role_family_category_gate_preset() {
        let assembled = assemble_system_prompt(&input(
            Role::Orchestrator,
            Some("bug"),
            Some(OVERLAY),
            Some(APPENDIX),
        ));

        let baseline = assembled
            .find(BASELINE)
            .expect("baseline が含まれるはずです");
        let family = assembled.find(FAMILY).expect("family が含まれるはずです");
        let overlay = assembled.find(OVERLAY).expect("overlay が含まれるはずです");
        let gate = assembled.find(GATE_MARK).expect("gate が含まれるはずです");
        let appendix = assembled
            .find(APPENDIX)
            .expect("appendix が含まれるはずです");
        assert!(baseline < family && family < overlay && overlay < gate && gate < appendix);
    }

    // Given: 同一の入力
    // When: assemble_system_prompt を 2 回呼ぶ
    // Then: 出力はバイト単位で同一になる
    #[test]
    fn assembly_is_byte_identical_for_identical_input() {
        let first = assemble_system_prompt(&input(Role::Orchestrator, Some("bug"), None, None));
        let second = assemble_system_prompt(&input(Role::Orchestrator, Some("bug"), None, None));

        assert_eq!(first, second);
    }

    // Given: 4 ロールそれぞれの入力
    // When: assemble_system_prompt する
    // Then: Intent Gate は Orchestrator にのみ現れる
    #[test]
    fn intent_gate_appears_only_for_orchestrator_across_role_matrix() {
        for role in [
            Role::Orchestrator,
            Role::Explorer,
            Role::Worker,
            Role::Reviewer,
        ] {
            let assembled = assemble_system_prompt(&input(role, None, None, None));
            assert_eq!(
                assembled.contains(GATE_MARK),
                role == Role::Orchestrator,
                "role = {}",
                role.name()
            );
        }
    }

    // Given: 任意レイヤー (overlay / appendix) が不存在の入力
    // When: assemble_system_prompt する
    // Then: 欠損レイヤー由来の区切り余白が残らない
    #[test]
    fn assembly_omits_absent_optional_layers_without_separator_artifacts() {
        let assembled = assemble_system_prompt(&input(Role::Explorer, None, None, None));

        assert_eq!(assembled, format!("{BASELINE}\n\n{FAMILY}"));
        assert!(!assembled.contains("\n\n\n"));
        assert!(!assembled.starts_with('\n'));
        assert!(!assembled.ends_with('\n'));
    }
}
