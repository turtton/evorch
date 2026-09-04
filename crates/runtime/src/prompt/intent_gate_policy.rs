//! Intent Gate 分類ポリシー (issue #69)。
//!
//! Intent Gate が参照する分類規則 — 8 分類軸・タスク種別判定表・実行形態
//! (ExecutionShape)・変異 (mutation) の持ち越し禁止ルール — を型と静的配列の
//! 単一ソースとして保持し、そこから 2 種のゲート本文をレンダリングする。
//!
//! - [`render_orchestrator_gate_body`]: Orchestrator 向け。エントリー事前
//!   ルーティングが選択した結果を検証する枠組みで書く。
//! - [`render_routing_gate_body`]: Layer A ルータ向け。8 項目を分類する指示の
//!   枠組みで書く。
//!
//! 両レンダラは同じ静的配列を順序走査する純粋関数であり、HashMap 等の
//! 非決定要素は一切使わない。同一入力に対してバイト単位で同一の出力を返す。

/// ゲート本文の見出し。両レンダラ共通。
const GATE_HEADING: &str = "## Intent Gate\n\n";

/// 分類軸表のヘッダ行。
const AXIS_TABLE_HEADER: &str = "| 分類軸 | 判定内容 |\n|---|---|\n";

/// タスク種別判定表の見出し行。
const TASK_KIND_TABLE_TITLE: &str = "タスク種別の判定表:\n\n";

/// タスク種別判定表のヘッダ行。
const TASK_KIND_TABLE_HEADER: &str = "| 種別 | 判定基準 |\n|---|---|\n";

/// 実行形態判定セクションの見出し行。
const EXECUTION_SHAPE_TITLE: &str = "実行形態 (ExecutionShape) の判定:\n\n";

/// Orchestrator 向け導入文。事前ルーティングの選択結果を検証する枠組み。
const ORCHESTRATOR_LEAD_IN: &str = "エントリー事前ルーティングにより タスク種別 と 実行形態 は既に選択されている。応答の前に、ユーザーの**現在のメッセージ**に対して次の 8 項目でその選択を検証すること。選択が現在のメッセージと整合しない場合は、検証結果に基づいて分類を訂正すること。";

/// ルーティング向け導入文。8 項目を分類する指示の枠組み。
const ROUTING_LEAD_IN: &str =
    "応答の前に、ユーザーの**現在のメッセージ**を次の 8 項目で分類すること。";

/// 変異 (mutation) の持ち越し禁止ルール。現在のメッセージごとの独立判定を強制する。
pub const MUTATION_NO_CARRYOVER_RULE: &str = "重要: 直前のターンで得た変異 (mutation) の許可は一切持ち越されない。ユーザーの現在のメッセージごとに、変異可否を含む全項目を独立して判定すること。";

/// 実行形態。単一ロールで完結するか、調整を伴うか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionShape {
    /// 単一ロールで完結できる形態。
    Direct,
    /// 調査の並列化・複数ロールの調整・委譲を伴う形態。
    Coordinated,
}

impl ExecutionShape {
    /// 表示名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Direct => "Direct",
            Self::Coordinated => "Coordinated",
        }
    }

    /// 選択基準の説明文。
    pub const fn criterion(self) -> &'static str {
        match self {
            Self::Direct => "単一ロールで完結でき、委譲が不要な場合に選択すること。",
            Self::Coordinated => {
                "調査の並列化、複数ロールの調整、委譲のいずれかが必要な場合に選択すること。"
            }
        }
    }
}

/// 全実行形態。プロンプトの列挙順。
pub const EXECUTION_SHAPES: [ExecutionShape; 2] =
    [ExecutionShape::Direct, ExecutionShape::Coordinated];

/// 分類軸 1 項目。軸名と判定内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassificationAxis {
    /// 軸名 (例: タスク種別)。
    pub name: &'static str,
    /// 判定内容。
    pub judgment: &'static str,
}

/// 8 分類軸。プロンプト表の行順。
pub const CLASSIFICATION_AXES: [ClassificationAxis; 8] = [
    ClassificationAxis {
        name: "タスク種別",
        judgment: "次の表から該当する種別を判定する",
    },
    ClassificationAxis {
        name: "必要ケイパビリティ",
        judgment: "判定結果に必要なツールと権限を列挙する",
    },
    ClassificationAxis {
        name: "変異可否",
        judgment: "ワークスペースへの書き込みが今回許されるかを判定する",
    },
    ClassificationAxis {
        name: "スコープ",
        judgment: "対象とするファイル・モジュール・調整範囲を特定する",
    },
    ClassificationAxis {
        name: "不確実性",
        judgment: "情報が不足している点とその確認方法を特定する",
    },
    ClassificationAxis {
        name: "期待する出力",
        judgment: "ユーザーが受け取るべき成果物の形を特定する",
    },
    ClassificationAxis {
        name: "完了条件",
        judgment: "何をもって完了とみなすかを判定する",
    },
    ClassificationAxis {
        name: "委譲要否",
        judgment: "他ロールへの委譲が必要かを判定する",
    },
];

/// タスク種別 1 項目。日本語ラベル・英語名・判定基準。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskKind {
    /// 日本語ラベル (例: 説明)。
    pub label_ja: &'static str,
    /// 英語名 (例: explain)。
    pub name_en: &'static str,
    /// 判定基準。
    pub criterion: &'static str,
}

/// 5 タスク種別。判定表の行順。
pub const TASK_KINDS: [TaskKind; 5] = [
    TaskKind {
        label_ja: "説明",
        name_en: "explain",
        criterion: "コードや仕組みの説明を求められている",
    },
    TaskKind {
        label_ja: "実装",
        name_en: "implement",
        criterion: "新しいコードや機能の作成を求められている",
    },
    TaskKind {
        label_ja: "調査",
        name_en: "look into",
        criterion: "読み取りと分析による状況把握を求められている",
    },
    TaskKind {
        label_ja: "修正",
        name_en: "broken",
        criterion: "壊れているものの修復を求められている",
    },
    TaskKind {
        label_ja: "リファクタ",
        name_en: "refactor",
        criterion: "振る舞いを変えない構造改善を求められている",
    },
];

/// Orchestrator 向け Intent Gate 本文をレンダリングする純粋関数。
///
/// keyTriggers ブロックは含まない。本文末尾は空行 1 つで終わり、
/// 呼び出し側はそこへマーク済みセクションを埋め込める。
pub fn render_orchestrator_gate_body() -> String {
    render_gate_body(ORCHESTRATOR_LEAD_IN)
}

/// Layer A ルータ向け Intent Gate 本文をレンダリングする純粋関数。
///
/// Orchestrator 向けと同じ静的配列を単一ソースとして使い、分類を指示する
/// 枠組みで書く。keyTriggers ブロックは含まない。
pub fn render_routing_gate_body() -> String {
    render_gate_body(ROUTING_LEAD_IN)
}

/// 導入文だけを差し替えて共通本文を組み立てる内部関数。
fn render_gate_body(lead_in: &str) -> String {
    let mut body = String::from(GATE_HEADING);
    body.push_str(lead_in);
    body.push_str("\n\n");
    body.push_str(AXIS_TABLE_HEADER);
    for axis in &CLASSIFICATION_AXES {
        body.push_str("| ");
        body.push_str(axis.name);
        body.push_str(" | ");
        body.push_str(axis.judgment);
        body.push_str(" |\n");
    }
    body.push('\n');
    body.push_str(TASK_KIND_TABLE_TITLE);
    body.push_str(TASK_KIND_TABLE_HEADER);
    for kind in &TASK_KINDS {
        body.push_str("| ");
        body.push_str(kind.label_ja);
        body.push_str(" (");
        body.push_str(kind.name_en);
        body.push_str(") | ");
        body.push_str(kind.criterion);
        body.push_str(" |\n");
    }
    body.push('\n');
    body.push_str(EXECUTION_SHAPE_TITLE);
    for shape in &EXECUTION_SHAPES {
        body.push_str("- ");
        body.push_str(shape.name());
        body.push_str(": ");
        body.push_str(shape.criterion());
        body.push('\n');
    }
    body.push('\n');
    body.push_str(MUTATION_NO_CARRYOVER_RULE);
    body.push_str("\n\n");
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 8 分類軸の静的配列
    // When: 軸名を順に取り出す
    // Then: ドキュメントどおりの 8 項目が決まった順序で並ぶ
    #[test]
    fn classification_axes_are_in_documented_order() {
        let names: Vec<&str> = CLASSIFICATION_AXES.iter().map(|axis| axis.name).collect();

        assert_eq!(
            names,
            vec![
                "タスク種別",
                "必要ケイパビリティ",
                "変異可否",
                "スコープ",
                "不確実性",
                "期待する出力",
                "完了条件",
                "委譲要否",
            ]
        );
    }

    // Given: 5 タスク種別の静的配列
    // When: 日本語ラベルと英語名を順に取り出す
    // Then: ドキュメントどおりの 5 種別が決まった順序で並ぶ
    #[test]
    fn task_kinds_are_in_documented_order() {
        let labels: Vec<(&str, &str)> = TASK_KINDS
            .iter()
            .map(|kind| (kind.label_ja, kind.name_en))
            .collect();

        assert_eq!(
            labels,
            vec![
                ("説明", "explain"),
                ("実装", "implement"),
                ("調査", "look into"),
                ("修正", "broken"),
                ("リファクタ", "refactor"),
            ]
        );
    }

    // Given: 実行形態の静的配列
    // When: 表示名を順に取り出す
    // Then: Direct / Coordinated が決まった順序で並ぶ
    #[test]
    fn execution_shapes_are_in_documented_order() {
        let names: Vec<&str> = EXECUTION_SHAPES.iter().map(|shape| shape.name()).collect();

        assert_eq!(names, vec!["Direct", "Coordinated"]);
    }

    // Given: 2 つのレンダラ (Orchestrator / routing)
    // When: それぞれの本文をレンダリングする
    // Then: 両者とも静的配列由来の全軸名・全判定内容・全種別ラベル・全判定基準・
    //       全実行形態・変異の持ち越し禁止ルールを含む (単一ソースの証明)
    #[test]
    fn both_renderers_draw_from_the_same_policy_arrays() {
        let orchestrator = render_orchestrator_gate_body();
        let routing = render_routing_gate_body();

        for body in [&orchestrator, &routing] {
            for axis in &CLASSIFICATION_AXES {
                assert!(body.contains(axis.name), "軸名不足: {}", axis.name);
                assert!(
                    body.contains(axis.judgment),
                    "判定内容不足: {}",
                    axis.judgment
                );
            }
            for kind in &TASK_KINDS {
                assert!(
                    body.contains(kind.label_ja),
                    "種別ラベル不足: {}",
                    kind.label_ja
                );
                assert!(
                    body.contains(kind.criterion),
                    "判定基準不足: {}",
                    kind.criterion
                );
            }
            for shape in &EXECUTION_SHAPES {
                assert!(
                    body.contains(shape.name()),
                    "実行形態不足: {}",
                    shape.name()
                );
                assert!(
                    body.contains(shape.criterion()),
                    "選択基準不足: {}",
                    shape.criterion()
                );
            }
            assert!(body.contains(MUTATION_NO_CARRYOVER_RULE));
        }
    }

    // Given: Orchestrator 向けゲート本文
    // When: render_orchestrator_gate_body する
    // Then: 再分類指示文は含まず、事前ルーティング選択を検証する枠組みで書かれる
    #[test]
    fn orchestrator_body_replaces_classification_instruction_with_verification_framing() {
        let body = render_orchestrator_gate_body();

        assert!(
            !body.contains(
                "応答の前に、ユーザーの**現在のメッセージ**を次の 8 項目で分類すること。"
            )
        );
        assert!(body.contains("事前ルーティング"));
        assert!(body.contains("既に選択されている"));
        assert!(body.contains("検証"));
    }

    // Given: ルーティング向けゲート本文
    // When: render_routing_gate_body する
    // Then: 8 項目を分類する指示の枠組みを保つ
    #[test]
    fn routing_body_keeps_classification_instruction() {
        let body = render_routing_gate_body();

        assert!(
            body.contains(
                "応答の前に、ユーザーの**現在のメッセージ**を次の 8 項目で分類すること。"
            )
        );
    }

    // Given: 各レンダラ
    // When: 2 回呼ぶ
    // Then: 出力はバイト単位で同一になる
    #[test]
    fn gate_bodies_are_deterministic_for_repeated_calls() {
        assert_eq!(
            render_orchestrator_gate_body(),
            render_orchestrator_gate_body()
        );
        assert_eq!(render_routing_gate_body(), render_routing_gate_body());
    }
}
