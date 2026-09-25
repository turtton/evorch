//! システムプロンプトの組立とロール別用途案内のテスト。

use agents::Role;
use runtime::prompt::default_role_triggers;
use runtime::{SystemPromptCatalog, SystemPromptCatalogError};

/// テスト用の完全カタログ。quick カテゴリと Orchestrator の appendix を持つ。
fn orchestrator_catalog() -> Result<SystemPromptCatalog, SystemPromptCatalogError> {
    SystemPromptCatalog::builder()
        .role_baseline(
            Role::Orchestrator,
            include_str!("../../config/assets/presets/role-orchestrator.md").trim_end(),
        )
        .role_baseline(
            Role::Explorer,
            "あなたは Explorer です。読み取り専用で調査を行い、ネットワークは明示的な\
             オプトイン時のみ許可します (ADR 0002)。",
        )
        .role_baseline(
            Role::Worker,
            "あなたは Worker です。ワークスペースの read-write を持ち、実装を担います \
             (ADR 0002)。",
        )
        .role_baseline(
            Role::Reviewer,
            "あなたは Reviewer です。生成とは独立したレビューを担います (ADR 0002)。",
        )
        .family_section(
            "family-claude",
            "Claude ファミリ向け規約: 応答は簡潔にし、ツール呼び出し前後の冗長な説明を\
             避けること。",
        )
        .family_section(
            "family-openai-reasoning",
            "o 系ファミリ向け規約: 推論の過程を本文に展開せず、結論と根拠を簡潔に返すこと。",
        )
        .family_section(
            "family-gpt5",
            "GPT-5 ファミリ向け規約: 指示の階層を明示的に解釈し、曖昧な点は先に質問すること。",
        )
        .family_section(
            "family-gemini",
            "Gemini ファミリ向け規約: 長い文脈からの引用元を明示すること。",
        )
        .family_section(
            "family-kimi",
            "Kimi ファミリ向け規約: 日本語応答では文体の揺れを避けること。",
        )
        .family_section(
            "family-generic",
            "汎用規約: 出力は単一の System プロンプトに従い、ロール境界を守ること。",
        )
        .category_overlay(
            "quick",
            "小規模な作業は変更範囲を絞り、必要な検証を行うこと。",
        )
        .appendix(
            Role::Orchestrator,
            "委譲先の報告は要約して共有し、原文の全文転記を避けること。",
        )
        .triggers(default_role_triggers())
        .build()
}

// Given: 完全なカタログ
// When: Orchestrator / カテゴリなし / claude-opus-4-1 でシステムプロンプトを解決する
// Then: Reviewer のトリガーは typed review 提出を含む正確なツール集合を公開する
#[test]
fn orchestrator_prompt_structure_matches_golden_fixture() {
    // Given: the actual role preset and the reviewed golden structure.
    let catalog = orchestrator_catalog().expect("catalog");
    let expected = include_str!("golden/system_prompt_orchestrator.txt");
    // When: assembling the orchestrator system prompt.
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("prompt");
    // Then: section routing and role purposes match without pinning all prose.
    let sections = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(sections(&prompt), sections(expected));
    let role_purposes = |text: &str| {
        text.lines()
            .filter(|line| line.starts_with("- Orchestrator:") || line.starts_with("- Worker:"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(role_purposes(&prompt), role_purposes(expected));
    assert!(!prompt.contains("許可ツール:"));
}

#[test]
fn orchestrator_prompt_disambiguates_role_purposes_before_delegation() {
    // Given: an orchestrator prompt assembled from the role preset.
    let catalog = orchestrator_catalog().expect("catalog");
    // When: resolving the Delegation Policy section.
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("prompt");
    // Then: the preset states a use/avoid pair per role, not just role names.
    assert!(
        prompt.contains("ロールの使い分け"),
        "role-use guidance marker missing"
    );
    assert!(
        prompt.contains("explorer=ローカルの探索・調査"),
        "explorer guidance missing"
    );
}

#[test]
fn orchestrator_prompt_delegates_detailed_work_to_children() {
    // Given: the shared orchestrator role baseline.
    let catalog = orchestrator_catalog().expect("catalog");
    // When: assembling the prompt with its family and routing sections.
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("prompt");
    // Then: the parent manages the goal rather than solving delegated work itself.
    for guidance in [
        "親は目的達成・優先順位付け・依存管理・受け入れ判断に集中し、詳細調査・比較検討・詳細計画は子ロールへ委譲する",
        "委譲前に自分で問題を解き切らず、自身の詳細調査・比較検討に時間を使わず委譲へ進む",
        "範囲が不明なら、範囲特定・分割案を explorer / planner へ委譲する",
        "目的・問い・制約・成果物・完了条件を明記し、手順の過固定を避ける",
        "委譲済み領域を親が並行して再調査しない",
        "管理上の判断に足る情報が揃ったら探索を止める",
    ] {
        assert!(
            prompt.contains(guidance),
            "delegation guidance missing: {guidance}"
        );
    }
}

#[test]
fn orchestrator_prompt_bounds_direct_work_and_acceptance_verification() {
    // Given: the orchestrator's limited direct-work exception.
    let catalog = orchestrator_catalog().expect("catalog");
    // When: assembling the prompt used for delegation and acceptance decisions.
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("prompt");
    // Then: delegated reports suffice, while verification remains focused and required.
    for guidance in [
        "単純な局所作業の直接実行は例外として許容する",
        "「単一ファイルの明白な変更」かつ「全体コンテキストを把握済み」の場合に限り、ロールの権限範囲を守る",
        "「把握済み」には委譲報告による把握を含み、親自身の事前深掘りを必要条件にしない",
        "作業中に範囲が拡大したら委譲に切り替える",
        "報告の不足・矛盾には、問いを絞った追加調査の委譲または reviewer による独立レビューで対応する",
        "自己申告だけを信用せず、変更ファイルとテスト結果を自分で検証する",
        "完了条件を満たす根拠の確認に絞り、受け入れ検証を全面再調査にしない",
    ] {
        assert!(
            prompt.contains(guidance),
            "direct-work or acceptance guidance missing: {guidance}"
        );
    }
}

#[test]
fn orchestrator_prompt_describes_reviewer_purpose_without_tool_roster() {
    let catalog = orchestrator_catalog().expect("catalog");
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("prompt");

    assert!(prompt.contains("- Reviewer: 実装結果を独立に検証する"));
    assert!(!prompt.contains("submit_review"));
}

// Given: quick overlay を登録した完全なカタログ
// When: Worker / quick / claude-opus-4-1 で解決する
// Then: Worker 用金標フィクスチャとバイト単位で一致する
#[test]
fn worker_quick_prompt_matches_golden_fixture() {
    let catalog = orchestrator_catalog().expect("カタログは構築できるはずです");
    let prompt = catalog
        .system_prompt_for(Role::Worker, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert_eq!(
        prompt,
        include_str!("golden/system_prompt_worker_quick.txt").trim_end()
    );
}
