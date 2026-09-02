//! Orchestrator システムプロンプト組立の金標テスト (issue #49 / AC10)。
//!
//! カタログ経由で解決した完全なプロンプトが、フィクスチャとバイト単位で
//! 一致することを検証する。フィクスチャ (`golden/system_prompt_orchestrator.txt`)
//! は組立出力そのものを格納し、末尾改行は含まない (組立の契約どおり)。

use agents::Role;
use runtime::prompt::default_role_triggers;
use runtime::{SystemPromptCatalog, SystemPromptCatalogError};

/// テスト用の完全カタログ。bug カテゴリと Orchestrator の appendix を持つ。
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
            "bug",
            "バグ対応カテゴリ: 根本原因の特定を最優先にし、修正前に再現手順を確定すること。",
        )
        .appendix(
            Role::Orchestrator,
            "委譲先の報告は要約して共有し、原文の全文転記を避けること。",
        )
        .triggers(default_role_triggers())
        .build()
}

// Given: 完全なカタログ
// When: Orchestrator / bug カテゴリ / claude-opus-4-1 でシステムプロンプトを解決する
// Then: 金標フィクスチャとバイト単位で一致する
#[test]
fn orchestrator_full_prompt_matches_golden_fixture() {
    let catalog = orchestrator_catalog().expect("カタログは構築できるはずです");
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, Some("bug"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert_eq!(prompt, GOLDEN_FIXTURE);
}

const GOLDEN_FIXTURE: &str = include_str!("golden/system_prompt_orchestrator.txt");
