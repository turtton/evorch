//! システムプロンプトカタログ (issue #49 / AC3, AC7, AC10)。
//!
//! ロール・ファミリ・カテゴリ別のプロンプト部品を保持し、構築時に必須部品の
//! 完全性を検証する。そのため provider 呼び出しより前に、欠落が型付きエラー
//! として表面化する (fail-closed by construction)。

use std::collections::BTreeMap;

use agents::Role;

use crate::prompt::assembly::{SystemPromptInput, assemble_system_prompt};
use crate::prompt::family::{ModelFamily, classify};
use crate::prompt::key_triggers::TriggerSource;

/// カタログの構築・参照で起こるエラー。
///
/// Display はロール名・キー名・カテゴリ名など識別子のみを含み、プロンプト
/// 本文を一切含まない。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SystemPromptCatalogError {
    /// 必須のロール baseline が未登録。
    #[error("ロール '{role}' のベースラインが登録されていません")]
    MissingRoleBaseline {
        /// 欠落していたロール名。
        role: String,
    },

    /// 必須のファミリセクションが未登録。
    #[error("ファミリセクション '{key}' が登録されていません")]
    MissingFamilySection {
        /// 欠落していたファミリセクションのキー。
        key: String,
    },

    /// 未登録のカテゴリが要求された。
    #[error("未知のカテゴリです: {category}")]
    UnknownCategory {
        /// 未登録だったカテゴリ名。
        category: String,
    },
}

/// 必須キー導出に使う [`ModelFamily`] の全 variant。
const ALL_FAMILIES: [ModelFamily; 6] = [
    ModelFamily::Claude,
    ModelFamily::OpenAiReasoning,
    ModelFamily::Gpt5,
    ModelFamily::Gemini,
    ModelFamily::Kimi,
    ModelFamily::Unknown,
];

/// 必須部品の完全性検証対象となるロール一覧 (ADR 0002)。
const ALL_ROLES: [Role; 4] = [
    Role::Orchestrator,
    Role::Explorer,
    Role::Worker,
    Role::Reviewer,
];

/// ロール・ファミリ・カテゴリ別のシステムプロンプト部品を保持するカタログ。
// allow: SIZE_OK — タスク仕様により unit tests をモジュール内に維持するため
// test コード込みで純 LOC が 250 を超える。production 本体のみなら 250 未満。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptCatalog {
    role_baselines: BTreeMap<String, String>,
    family_sections: BTreeMap<String, String>,
    category_overlays: BTreeMap<String, String>,
    appendices: BTreeMap<String, String>,
    category_appendices: BTreeMap<(String, String), String>,
    triggers: Vec<TriggerSource>,
}

impl SystemPromptCatalog {
    /// [`SystemPromptCatalogBuilder`] を開始する。
    pub fn builder() -> SystemPromptCatalogBuilder {
        SystemPromptCatalogBuilder::default()
    }

    /// システムプロンプトを解決する純粋関数 (AC3, AC10)。
    ///
    /// - `model_id` からファミリを分類し、対応する family section を選ぶ。
    /// - `category` が指定され、未登録なら型付きエラーになる。
    /// - appendix はロール名をキーに内部 lookup し、未登録なら省略する。
    ///   `(role, category)` でカテゴリスコープの appendix
    ///   ([`SystemPromptCatalogBuilder::category_appendix`]) が登録されて
    ///   いればロールレベルより優先する。これは config の per-field
    ///   category-beats-role マージ規約と同じ優先順位である。
    ///   設計判断: appendix のキーはこのシグネチャから導出できるロール名と
    ///   する。resolver がよりリッチなキーを持つテキストを解決済みで渡す
    ///   ケースは [`assemble_system_prompt`] を直接使う。
    pub fn system_prompt_for(
        &self,
        role: Role,
        category: Option<&str>,
        model_id: &str,
    ) -> Result<String, SystemPromptCatalogError> {
        let family = classify(model_id);
        let role_baseline = self.role_baselines.get(role.name()).ok_or_else(|| {
            SystemPromptCatalogError::MissingRoleBaseline {
                role: role.name().to_owned(),
            }
        })?;
        let family_section = self
            .family_sections
            .get(family.base_section_key())
            .ok_or_else(|| SystemPromptCatalogError::MissingFamilySection {
                key: family.base_section_key().to_owned(),
            })?;
        let category_overlay = category
            .map(|category| {
                self.category_overlays
                    .get(category)
                    .map(String::as_str)
                    .ok_or_else(|| SystemPromptCatalogError::UnknownCategory {
                        category: category.to_owned(),
                    })
            })
            .transpose()?;
        let scoped_appendix = category.and_then(|category| {
            self.category_appendices
                .get(&(role.name().to_lowercase(), category.to_owned()))
        });
        let appendix = scoped_appendix
            .or_else(|| self.appendices.get(role.name()))
            .map(String::as_str);
        Ok(assemble_system_prompt(&SystemPromptInput {
            role,
            category,
            family,
            role_baseline,
            family_section,
            category_overlay,
            appendix,
            triggers: &self.triggers,
        }))
    }
}

/// [`SystemPromptCatalog`] のビルダー。部品を登録し、`build` で完全性を
/// 検証した上でカタログを構築する。
#[derive(Debug, Default)]
pub struct SystemPromptCatalogBuilder {
    role_baselines: BTreeMap<String, String>,
    family_sections: BTreeMap<String, String>,
    category_overlays: BTreeMap<String, String>,
    appendices: BTreeMap<String, String>,
    category_appendices: BTreeMap<(String, String), String>,
    triggers: Vec<TriggerSource>,
}

impl SystemPromptCatalogBuilder {
    /// ロールの baseline セクションを登録する (4 ロール全分が必須)。
    pub fn role_baseline(mut self, role: Role, text: impl Into<String>) -> Self {
        self.role_baselines
            .insert(role.name().to_owned(), text.into());
        self
    }

    /// ファミリセクションを登録する (6 キー全てが必須)。
    pub fn family_section(mut self, key: impl Into<String>, text: impl Into<String>) -> Self {
        self.family_sections.insert(key.into(), text.into());
        self
    }

    /// カテゴリ overlay を登録する (任意。未登録カテゴリの参照はエラー)。
    pub fn category_overlay(
        mut self,
        category: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        self.category_overlays.insert(category.into(), text.into());
        self
    }

    /// ロールの appendix を登録する (任意)。
    pub fn appendix(mut self, role: Role, text: impl Into<String>) -> Self {
        self.appendices.insert(role.name().to_owned(), text.into());
        self
    }

    /// カテゴリスコープの appendix を登録する (任意)。
    ///
    /// `(ロール名小文字, category)` をキーに保持し、`system_prompt_for` で
    /// 同一ロールのロールレベル appendix ([`Self::appendix`]) より優先される。
    pub fn category_appendix(
        mut self,
        role: Role,
        category: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        self.category_appendices
            .insert((role.name().to_lowercase(), category.into()), text.into());
        self
    }

    /// keyTriggers のソース一覧を登録する (既定は空)。
    pub fn triggers(mut self, triggers: Vec<TriggerSource>) -> Self {
        self.triggers = triggers;
        self
    }

    /// 必須部品の完全性を検証し、カタログを構築する。
    ///
    /// 検証はロール (固定順) → ファミリ ([`ALL_FAMILIES`] 順) のどちらか
    /// 最初の欠落を型付きエラーで報告する。
    pub fn build(self) -> Result<SystemPromptCatalog, SystemPromptCatalogError> {
        for role in ALL_ROLES {
            if !self.role_baselines.contains_key(role.name()) {
                return Err(SystemPromptCatalogError::MissingRoleBaseline {
                    role: role.name().to_owned(),
                });
            }
        }
        for family in ALL_FAMILIES {
            let key = family.base_section_key();
            if !self.family_sections.contains_key(key) {
                return Err(SystemPromptCatalogError::MissingFamilySection {
                    key: key.to_owned(),
                });
            }
        }
        Ok(SystemPromptCatalog {
            role_baselines: self.role_baselines,
            family_sections: self.family_sections,
            category_overlays: self.category_overlays,
            appendices: self.appendices,
            category_appendices: self.category_appendices,
            triggers: self.triggers,
        })
    }
}

#[cfg(test)]
mod tests {
    use agents::Role;

    use crate::prompt::SystemPromptCatalog;
    use crate::prompt::SystemPromptCatalogBuilder;
    use crate::prompt::SystemPromptCatalogError;
    use crate::prompt::TriggerSource;

    const SENTINEL: &str = "PROMPT-BODY-SENTINEL-この文字列は本文です";

    fn complete_builder() -> SystemPromptCatalogBuilder {
        SystemPromptCatalog::builder()
            .role_baseline(Role::Orchestrator, format!("ORCH-BASELINE {SENTINEL}"))
            .role_baseline(Role::Explorer, format!("EXPLORER-BASELINE {SENTINEL}"))
            .role_baseline(Role::Worker, format!("WORKER-BASELINE {SENTINEL}"))
            .role_baseline(Role::Reviewer, format!("REVIEWER-BASELINE {SENTINEL}"))
            .family_section("family-claude", format!("CLAUDE-SECTION {SENTINEL}"))
            .family_section(
                "family-openai-reasoning",
                format!("REASONING-SECTION {SENTINEL}"),
            )
            .family_section("family-gpt5", format!("GPT5-SECTION {SENTINEL}"))
            .family_section("family-gemini", format!("GEMINI-SECTION {SENTINEL}"))
            .family_section("family-kimi", format!("KIMI-SECTION {SENTINEL}"))
            .family_section("family-generic", format!("GENERIC-SECTION {SENTINEL}"))
            .category_overlay("bug", format!("BUG-OVERLAY {SENTINEL}"))
            .appendix(Role::Orchestrator, format!("ORCH-APPENDIX {SENTINEL}"))
            .triggers(vec![TriggerSource {
                name: "Orchestrator".to_owned(),
                description: "orchestrator-summary".to_owned(),
            }])
    }

    // Given: 必須部品が欠けたビルダー (Worker baseline 欠落)
    // When: build する
    // Then: 型付きエラーで欠落ロール名を報告する
    #[test]
    fn catalog_build_fails_typed_when_required_text_missing() {
        let missing_role = SystemPromptCatalog::builder()
            .role_baseline(Role::Orchestrator, "ORCH")
            .role_baseline(Role::Explorer, "EXPLORER")
            .role_baseline(Role::Reviewer, "REVIEWER")
            .build();
        assert!(matches!(
            missing_role,
            Err(SystemPromptCatalogError::MissingRoleBaseline { role }) if role == "Worker"
        ));

        let missing_family = SystemPromptCatalog::builder()
            .role_baseline(Role::Orchestrator, "ORCH")
            .role_baseline(Role::Explorer, "EXPLORER")
            .role_baseline(Role::Worker, "WORKER")
            .role_baseline(Role::Reviewer, "REVIEWER")
            .family_section("family-claude", "CLAUDE")
            .family_section("family-openai-reasoning", "REASONING")
            .family_section("family-gpt5", "GPT5")
            .family_section("family-kimi", "KIMI")
            .family_section("family-generic", "GENERIC")
            .build();
        assert!(matches!(
            missing_family,
            Err(SystemPromptCatalogError::MissingFamilySection { key }) if key == "family-gemini"
        ));
    }

    // Given: 登録本文にセンチネルを含むカタログでエラーを起こす
    // When: エラーの Display を見る
    // Then: 本文 (センチネル) を一切含まない
    #[test]
    fn catalog_error_display_never_contains_prompt_body() {
        let build_error = SystemPromptCatalog::builder()
            .role_baseline(Role::Orchestrator, format!("ORCH {SENTINEL}"))
            .role_baseline(Role::Explorer, format!("EXPLORER {SENTINEL}"))
            .role_baseline(Role::Worker, format!("WORKER {SENTINEL}"))
            .role_baseline(Role::Reviewer, format!("REVIEWER {SENTINEL}"))
            .family_section("family-claude", format!("CLAUDE {SENTINEL}"))
            .build()
            .expect_err("family 欠落で失敗するはずです");
        assert!(!build_error.to_string().contains(SENTINEL));

        let catalog = complete_builder()
            .build()
            .expect("完全なカタログは構築できるはずです");
        let unknown_category = catalog
            .system_prompt_for(Role::Explorer, Some("no-such-category"), "claude-opus-4-1")
            .expect_err("未登録カテゴリで失敗するはずです");
        assert!(!unknown_category.to_string().contains(SENTINEL));
    }

    // Given: ファミリごとに異なるセクション本文を持つカタログ
    // When: model_id を変えて system_prompt_for する
    // Then: 分類されたファミリのセクション本文が選択される
    #[test]
    fn system_prompt_for_selects_family_section_by_model_id() {
        let catalog = complete_builder()
            .build()
            .expect("完全なカタログは構築できるはずです");

        let cases = [
            ("claude-opus-4-1", "CLAUDE-SECTION"),
            ("gpt-5", "GPT5-SECTION"),
            ("o3-mini", "REASONING-SECTION"),
            ("gemini-2.5-pro", "GEMINI-SECTION"),
            ("kimi-k2", "KIMI-SECTION"),
            ("unknown-model", "GENERIC-SECTION"),
        ];
        for (model_id, section_marker) in cases {
            let prompt = catalog
                .system_prompt_for(Role::Explorer, None, model_id)
                .expect("登録済みの部品のみを参照するはずです");
            assert!(
                prompt.contains(section_marker),
                "model_id = {model_id} は {section_marker} を選ぶはずです"
            );
        }
    }

    // Given: 完全なカタログ
    // When: 未登録のカテゴリで system_prompt_for する
    // Then: 型付きエラーでカテゴリ名を報告する
    #[test]
    fn system_prompt_for_unknown_category_is_typed_error() {
        let catalog = complete_builder()
            .build()
            .expect("完全なカタログは構築できるはずです");

        let error = catalog
            .system_prompt_for(Role::Worker, Some("no-such-category"), "claude-opus-4-1")
            .expect_err("未登録カテゴリで失敗するはずです");
        assert!(matches!(
            error,
            SystemPromptCatalogError::UnknownCategory { category } if category == "no-such-category"
        ));
    }

    // Given: ロールレベル appendix と Orchestrator/bug スコープ appendix の両方を
    //        持つカタログ
    // When: Orchestrator / bug で system_prompt_for する
    // Then: カテゴリスコープの本文が採用され、ロールレベルの本文は置き換わる
    //       (config の per-field category-beats-role 規約と同じ優先順位)
    #[test]
    fn system_prompt_for_prefers_category_scoped_appendix_over_role_level() {
        let catalog = complete_builder()
            .category_appendix(
                Role::Orchestrator,
                "bug",
                format!("SCOPED-APPENDIX {SENTINEL}"),
            )
            .build()
            .expect("完全なカタログは構築できるはずです");

        let prompt = catalog
            .system_prompt_for(Role::Orchestrator, Some("bug"), "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");

        assert!(
            prompt.ends_with(&format!("SCOPED-APPENDIX {SENTINEL}")),
            "appendix レイヤーはカテゴリスコープの本文のはずです"
        );
        assert!(
            !prompt.contains(&format!("ORCH-APPENDIX {SENTINEL}")),
            "ロールレベルの本文はスコープ優先で置き換わるはずです"
        );
    }

    // Given: ロールレベル appendix のみを持ち bug スコープは未登録のカタログ
    // When: Orchestrator / bug で system_prompt_for する
    // Then: ロールレベルの本文にフォールバックする
    #[test]
    fn system_prompt_for_falls_back_to_role_level_appendix_without_scoped_one() {
        let catalog = complete_builder()
            .build()
            .expect("完全なカタログは構築できるはずです");

        let prompt = catalog
            .system_prompt_for(Role::Orchestrator, Some("bug"), "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");

        assert!(
            prompt.ends_with(&format!("ORCH-APPENDIX {SENTINEL}")),
            "スコープ未登録ならロールレベルの本文のはずです"
        );
    }

    // Given: Orchestrator/bug スコープ appendix を追加登録したカタログ
    // When: category None で system_prompt_for する
    // Then: ロールレベルの本文が採用され、スコープ登録の影響を受けない
    #[test]
    fn system_prompt_for_ignores_category_scoped_appendix_when_category_is_none() {
        let catalog = complete_builder()
            .category_appendix(
                Role::Orchestrator,
                "bug",
                format!("SCOPED-APPENDIX {SENTINEL}"),
            )
            .build()
            .expect("完全なカタログは構築できるはずです");

        let prompt = catalog
            .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");

        assert!(
            prompt.ends_with(&format!("ORCH-APPENDIX {SENTINEL}")),
            "category None ではロールレベルの本文のはずです"
        );
        assert!(
            !prompt.contains("SCOPED-APPENDIX"),
            "category None ではスコープの本文は現れないはずです"
        );
    }
}
