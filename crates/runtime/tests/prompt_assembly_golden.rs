//! システムプロンプトの組立とロール別ツール公開契約のテスト。

use agents::Role;
use runtime::prompt::default_role_triggers;
use runtime::{SystemPromptCatalog, SystemPromptCatalogError};

/// テスト用の完全カタログ。quick カテゴリと Orchestrator の appendix を持つ。
fn orchestrator_catalog() -> Result<SystemPromptCatalog, SystemPromptCatalogError> {
    SystemPromptCatalog::builder()
        .role_baseline(
            Role::Orchestrator,
            "あなたは Orchestrator です。委譲と AgentRun 間メッセージによる調整を担い、\
             mutation tool は持ちません (ADR 0002)。",
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
fn orchestrator_prompt_exposes_typed_reviewer_tools() {
    let catalog = orchestrator_catalog().expect("カタログは構築できるはずです");
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    let reviewer = prompt
        .lines()
        .find(|line| line.starts_with("- Reviewer:"))
        .expect("reviewer routing trigger");
    let (_, capabilities) = reviewer.split_once(':').expect("role separator");
    let (_, tools) = capabilities.split_once(':').expect("tool list separator");
    let tools = tools.split('/').next().expect("tool list");
    assert_eq!(
        tools.split(',').map(str::trim).collect::<Vec<_>>(),
        [
            "git_diff",
            "grep",
            "ledger_append",
            "ledger_read",
            "read",
            "submit_review"
        ]
    );
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
