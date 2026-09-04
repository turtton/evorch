//! Intent Gate セクションのレンダラ (issue #49 / AC8, issue #69)。
//!
//! 本文は [`crate::prompt::intent_gate_policy`] の型と静的配列から
//! レンダリングする。Orchestrator 向け本文は、エントリー事前ルーティングが
//! 選択した分類を現在のメッセージに対して検証する枠組みで書かれる。
//! 内容は keyTriggers ブロック (AC9) をマーク済みセクションとして埋め込む。

use crate::prompt::intent_gate_policy::render_orchestrator_gate_body;
use crate::prompt::key_triggers::{TriggerSource, render_key_triggers};

/// keyTriggers 埋め込みセクションの開始マーカー。
const KEY_TRIGGERS_BEGIN: &str = "<!-- keyTriggers BEGIN -->";

/// keyTriggers 埋め込みセクションの終了マーカー。
const KEY_TRIGGERS_END: &str = "<!-- keyTriggers END -->";

/// Intent Gate セクションをレンダリングする純粋関数。
///
/// Orchestrator 向けのゲート本文 ([`render_orchestrator_gate_body`]) の末尾に、
/// [`render_key_triggers`] の出力をマーク済みセクションとして埋め込む。
/// 出力に末尾の改行は含まない。
pub fn render_intent_gate(triggers: &[TriggerSource]) -> String {
    let mut rendered = render_orchestrator_gate_body();
    rendered.push_str(KEY_TRIGGERS_BEGIN);
    rendered.push('\n');
    rendered.push_str(&render_key_triggers(triggers));
    rendered.push('\n');
    rendered.push_str(KEY_TRIGGERS_END);
    rendered
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

    // Given: 実装された Intent Gate
    // When: render_intent_gate する
    // Then: 金標テキストとバイト単位で一致する
    #[test]
    fn intent_gate_golden_matches_expected_bytes() {
        let rendered = render_intent_gate(&[]);

        assert_eq!(rendered, INTENT_GATE_GOLDEN);
    }

    // Given: 実装された Intent Gate
    // When: render_intent_gate する
    // Then: 分類必須の 8 項目と Direct / Coordinated に言及する
    #[test]
    fn intent_gate_mentions_all_required_classification_fields() {
        let rendered = render_intent_gate(&[]);

        for keyword in [
            "タスク種別",
            "必要ケイパビリティ",
            "変異可否",
            "スコープ",
            "不確実性",
            "期待する出力",
            "完了条件",
            "委譲要否",
            "Direct",
            "Coordinated",
        ] {
            assert!(rendered.contains(keyword), "キーワード不足: {keyword}");
        }
    }

    // Given: 実装された Intent Gate
    // When: render_intent_gate する
    // Then: 現在のメッセージのみを対象とし、変異許可の持ち越しを否定する
    #[test]
    fn intent_gate_declares_current_message_only_and_no_mutation_carryover() {
        let rendered = render_intent_gate(&[]);

        assert!(rendered.contains("現在のメッセージ"));
        assert!(rendered.contains("持ち越されない"));
        assert!(rendered.contains("独立して"));
    }

    // Given: トリガー 1 件
    // When: render_intent_gate する
    // Then: keyTriggers ブロックがマーク済みセクションに埋め込まれる
    #[test]
    fn intent_gate_embeds_key_triggers_from_sources() {
        let rendered = render_intent_gate(&[source("TestRole", "test-description")]);

        assert!(rendered.contains("<!-- keyTriggers BEGIN -->"));
        assert!(rendered.contains("<!-- keyTriggers END -->"));
        assert!(rendered.contains("### keyTriggers"));
        assert!(rendered.contains("- TestRole: test-description"));
    }

    // Given: Orchestrator 向け Intent Gate
    // When: render_intent_gate する
    // Then: 再分類指示文は検証枠組みに置き換わり、keyTriggers だけが埋め込まれる
    #[test]
    fn intent_gate_uses_verification_framing_from_policy_module() {
        let rendered = render_intent_gate(&[]);

        assert!(!rendered.contains("次の 8 項目で分類すること"));
        assert!(rendered.contains("事前ルーティング"));
        assert!(rendered.contains("検証"));
    }

    // 金標 (空トリガー時の render_intent_gate 出力とバイト一致させる)。
    const INTENT_GATE_GOLDEN: &str = r#"## Intent Gate

エントリー事前ルーティングにより タスク種別 と 実行形態 は既に選択されている。応答の前に、ユーザーの**現在のメッセージ**に対して次の 8 項目でその選択を検証すること。選択が現在のメッセージと整合しない場合は、検証結果に基づいて分類を訂正すること。

| 分類軸 | 判定内容 |
|---|---|
| タスク種別 | 次の表から該当する種別を判定する |
| 必要ケイパビリティ | 判定結果に必要なツールと権限を列挙する |
| 変異可否 | ワークスペースへの書き込みが今回許されるかを判定する |
| スコープ | 対象とするファイル・モジュール・調整範囲を特定する |
| 不確実性 | 情報が不足している点とその確認方法を特定する |
| 期待する出力 | ユーザーが受け取るべき成果物の形を特定する |
| 完了条件 | 何をもって完了とみなすかを判定する |
| 委譲要否 | 他ロールへの委譲が必要かを判定する |

タスク種別の判定表:

| 種別 | 判定基準 |
|---|---|
| 説明 (explain) | コードや仕組みの説明を求められている |
| 実装 (implement) | 新しいコードや機能の作成を求められている |
| 調査 (look into) | 読み取りと分析による状況把握を求められている |
| 修正 (broken) | 壊れているものの修復を求められている |
| リファクタ (refactor) | 振る舞いを変えない構造改善を求められている |

実行形態 (ExecutionShape) の判定:

- Direct: 単一ロールで完結でき、委譲が不要な場合に選択すること。
- Coordinated: 調査の並列化、複数ロールの調整、委譲のいずれかが必要な場合に選択すること。

重要: 直前のターンで得た変異 (mutation) の許可は一切持ち越されない。ユーザーの現在のメッセージごとに、変異可否を含む全項目を独立して判定すること。

<!-- keyTriggers BEGIN -->
### keyTriggers

(該当なし)

該当するトリガーが存在する場合のみ、対応するセクションを参照すること。
<!-- keyTriggers END -->"#;
}
