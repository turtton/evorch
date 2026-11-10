//! 編集ツール (`edit_tool`) の設定・既定判定・保存の契約。

use std::path::{Path, PathBuf};

use config::save::save_openai_compatible_provider_edit;
use config::types::provider::ModelEntryConfig;
use config::{
    CodexProviderInput, Config, ConfigError, EditToolConfig, EditToolSelection, LoadOptions,
    OpenAiCompatibleProviderInput, ProviderCredentialInput, ProviderProfileConfig,
    ProviderTypeConfig, gpt_generation, save_codex_provider_edit,
};

fn config_path(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir.join(".evorch")).expect("config directory");
    dir.join(".evorch/config.toml")
}

fn options(dir: &Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(dir.join(".evorch")),
        read_env: false,
        ..LoadOptions::default()
    }
}

fn load_text(content: &str, strict: bool) -> Result<Config, ConfigError> {
    let tmp = tempfile::tempdir().expect("temporary directory");
    std::fs::write(config_path(tmp.path()), content).expect("write config");
    if strict {
        Config::load_strict(&options(tmp.path()))
    } else {
        Config::load(&options(tmp.path()))
    }
}

fn profile(content: &str) -> ProviderProfileConfig {
    toml::from_str(content).expect("profile parses")
}

fn compatible_input(name: &str) -> OpenAiCompatibleProviderInput {
    OpenAiCompatibleProviderInput {
        name: name.into(),
        provider_type: ProviderTypeConfig::OpenAiCompatible,
        base_url: "https://example.com/v1".into(),
        credential: ProviderCredentialInput::Env {
            var: "API_KEY".into(),
        },
        models: vec![ModelEntryConfig::enabled("gpt-5")],
        excluded_models: vec![],
        default_model: "gpt-5".into(),
    }
}

fn codex_input(name: &str) -> CodexProviderInput {
    CodexProviderInput {
        name: name.into(),
        account: name.into(),
        base_url: "https://chatgpt.com/backend-api/codex".into(),
        models: vec![ModelEntryConfig::enabled("gpt-5.1-codex")],
        default_model: "gpt-5.1-codex".into(),
    }
}

const HAND_WRITTEN: &str = r#"version = 3

[providers.old]
type = "openai-compatible"
base_url = "https://example.com/v1"
api_key_env = "API_KEY"
models = ["gpt-5"]
default_model = "gpt-5"
edit_tool = "apply-patch"
"#;

#[test]
fn string_models_use_the_default_selection() {
    for strict in [true, false] {
        // Given: 文字列形式のモデルだけを持つ OpenAI プロファイル
        let config = load_text(
            "[providers.oa]\ntype = 'openai'\nmodels = ['gpt-5.1-codex', 'gpt-4.1']\ndefault_model = 'gpt-5.1-codex'\n",
            strict,
        )
        .expect("string models load");
        let profile = &config.providers["oa"];

        // When / Then: 上書きはなく、既定判定で解決される
        assert_eq!(profile.edit_tool, None);
        assert!(profile.models.iter().all(|entry| entry.edit_tool.is_none()));
        assert_eq!(
            profile.edit_tool_for("gpt-5.1-codex"),
            EditToolConfig::ApplyPatch
        );
        assert_eq!(profile.edit_tool_for("gpt-4.1"), EditToolConfig::Edit);
    }
}

#[test]
fn table_model_and_profile_edit_tool_load_strictly_and_tolerantly() {
    for strict in [true, false] {
        // Given: profile と model entry の両方に edit_tool を書いた設定
        let config = load_text(
            r#"
[providers.oa]
type = "openai"
edit_tool = "edit"
default_model = "gpt-5.5"
models = [
  { id = "gpt-5.5", enabled = true, edit_tool = "apply-patch" },
  "gpt-6-astra",
]

[providers.compat]
type = "openai-compatible"
base_url = "https://example.com/v1"
models = [{ id = "local", enabled = true, edit_tool = "apply-patch" }]
default_model = "local"
"#,
            strict,
        )
        .expect("edit_tool is a known field");

        // Then: それぞれの位置の値が読み取られる
        let openai = &config.providers["oa"];
        assert_eq!(openai.edit_tool, Some(EditToolConfig::Edit));
        assert_eq!(openai.models[0].edit_tool, Some(EditToolConfig::ApplyPatch));
        assert_eq!(openai.models[1].edit_tool, None);
        let compatible = &config.providers["compat"];
        assert_eq!(compatible.edit_tool, None);
        assert_eq!(
            compatible.models[0].edit_tool,
            Some(EditToolConfig::ApplyPatch)
        );
    }
}

#[test]
fn invalid_edit_tool_values_are_rejected_with_their_paths() {
    let cases = [
        (
            "[providers.oa]\ntype = 'openai'\nedit_tool = 'patch'\n",
            "providers.oa.edit_tool",
        ),
        (
            "[providers.oa]\ntype = 'openai'\nmodels = [{ id = 'gpt-5', enabled = true, edit_tool = 'patch' }]\ndefault_model = 'gpt-5'\n",
            "providers.oa.models[0].edit_tool",
        ),
        (
            "[providers.oa]\ntype = 'openai'\nedit_tool = 1\n",
            "providers.oa.edit_tool",
        ),
    ];
    for (document, expected) in cases {
        for strict in [true, false] {
            // When: 不正な値を読み込む
            let error = load_text(document, strict).expect_err("invalid edit_tool is rejected");

            // Then: パス付きの InvalidField になる
            assert!(
                matches!(&error, ConfigError::InvalidField { path, message }
                    if path == expected && message.contains("apply-patch") && message.contains("edit")),
                "{error}"
            );
        }
    }
}

#[test]
fn model_override_wins_over_profile_and_default() {
    // Given: profile は edit、一部のモデルだけ apply-patch
    let profile = profile(
        r#"
type = "openai"
edit_tool = "edit"
models = [{ id = "gpt-4.1", enabled = true, edit_tool = "apply-patch" }, "gpt-6-astra"]
default_model = "gpt-4.1"
"#,
    );

    // Then: model > profile > default の順に解決される
    assert_eq!(profile.edit_tool_for("gpt-4.1"), EditToolConfig::ApplyPatch);
    assert_eq!(profile.edit_tool_for("gpt-6-astra"), EditToolConfig::Edit);
    assert_eq!(profile.edit_tool_for("unlisted"), EditToolConfig::Edit);

    // And: profile の指定がなければ既定判定に落ちる
    let without_profile = ProviderProfileConfig {
        edit_tool: None,
        ..profile
    };
    assert_eq!(
        without_profile.edit_tool_for("gpt-6-astra"),
        EditToolConfig::ApplyPatch
    );
    assert_eq!(
        without_profile.edit_tool_for("gpt-4.1"),
        EditToolConfig::ApplyPatch
    );
}

#[test]
fn fast_variants_find_their_base_entry() {
    // Given: base ID のエントリだけが edit_tool を持つ Codex プロファイル
    let profile = profile(
        r#"
type = "openai-codex"
models = [
  { id = "gpt-5.1-codex", enabled = true, edit_tool = "edit" },
  { id = "gpt-6-astra+fast", enabled = true, edit_tool = "edit" },
  "gpt-6-astra",
]
default_model = "gpt-5.1-codex"
"#,
    );

    // Then: +fast を除いた ID で再検索し、完全一致があればそれを優先する
    assert_eq!(
        profile.edit_tool_for("gpt-5.1-codex+fast"),
        EditToolConfig::Edit
    );
    assert_eq!(
        profile.edit_tool_for("gpt-6-astra+fast"),
        EditToolConfig::Edit
    );
    assert_eq!(
        profile.edit_tool_for("gpt-6-astra"),
        EditToolConfig::ApplyPatch
    );
    assert_eq!(
        profile.edit_tool_for("gpt-5.5+fast"),
        EditToolConfig::ApplyPatch
    );
    assert_eq!(
        profile.edit_tool_for("GPT-5.5+fast"),
        EditToolConfig::ApplyPatch
    );
}

#[test]
fn default_selection_targets_gpt_5_and_later_on_openai_providers() {
    for provider_type in [ProviderTypeConfig::OpenAi, ProviderTypeConfig::OpenAiCodex] {
        for model in [
            "gpt-5",
            "gpt-5.1-codex",
            "gpt-6-astra",
            "gpt-5.6-sol+fast",
            "GPT-5",
            "Gpt-6-Astra",
        ] {
            assert_eq!(
                EditToolConfig::default_for(provider_type, model),
                EditToolConfig::ApplyPatch,
                "{provider_type:?} {model}"
            );
        }
        for model in [
            "gpt-4.1",
            "o3",
            "gpt-oss-120b",
            "codex-mini-latest",
            "GPT-4.1",
            "GPT-OSS-120B",
            "gpt-",
        ] {
            assert_eq!(
                EditToolConfig::default_for(provider_type, model),
                EditToolConfig::Edit,
                "{provider_type:?} {model}"
            );
        }
    }
    for provider_type in [
        ProviderTypeConfig::OpenAiCompatible,
        ProviderTypeConfig::Openrouter,
        ProviderTypeConfig::GithubCopilot,
        ProviderTypeConfig::Cursor,
        ProviderTypeConfig::Anthropic,
    ] {
        assert_eq!(
            EditToolConfig::default_for(provider_type, "gpt-5"),
            EditToolConfig::Edit,
            "{provider_type:?}"
        );
    }
}

#[test]
fn gpt_generation_reads_the_major_version_only() {
    assert_eq!(gpt_generation("gpt-5"), Some(5));
    assert_eq!(gpt_generation("gpt-5.1-codex"), Some(5));
    assert_eq!(gpt_generation("gpt-6-astra"), Some(6));
    assert_eq!(gpt_generation("gpt-4.1"), Some(4));
    // runtime の model family 判定と同じく、大文字小文字を区別しない。
    assert_eq!(gpt_generation("GPT-5"), Some(5));
    assert_eq!(gpt_generation("Gpt-5.1-Codex"), Some(5));
    assert_eq!(gpt_generation("GPT-4.1"), Some(4));
    for model in [
        "gpt-oss-120b",
        "GPT-OSS-120B",
        "o3",
        "codex-mini-latest",
        "gpt-",
        "gpt5",
    ] {
        assert_eq!(gpt_generation(model), None, "{model}");
    }
}

#[test]
fn selection_resolves_like_the_profile_and_defaults_to_no_overrides() {
    // Given: profile と model の上書きを持つプロファイル
    let profile = profile(
        r#"
type = "openai-codex"
edit_tool = "edit"
models = [{ id = "gpt-5.5", enabled = true, edit_tool = "apply-patch" }, "gpt-6-astra"]
default_model = "gpt-5.5"
"#,
    );

    // When: routing へ渡す選択に変換する
    let selection = profile.edit_tool_selection();

    // Then: 上書きだけを保持し、同じ解決順で判定する
    assert_eq!(
        selection,
        EditToolSelection {
            profile: Some(EditToolConfig::Edit),
            models: [("gpt-5.5".to_owned(), EditToolConfig::ApplyPatch)].into(),
        }
    );
    for model in ["gpt-5.5", "gpt-5.5+fast", "gpt-6-astra", "unlisted"] {
        assert_eq!(
            selection.for_model(profile.provider_type, model),
            profile.edit_tool_for(model),
            "{model}"
        );
    }
    // And: 上書きなしの既定値はどの provider type でも既定判定になる
    let empty = EditToolSelection::default();
    assert_eq!(
        empty.for_model(ProviderTypeConfig::OpenAiCodex, "gpt-6-astra"),
        EditToolConfig::ApplyPatch
    );
    assert_eq!(
        empty.for_model(ProviderTypeConfig::OpenAiCompatible, "gpt-6-astra"),
        EditToolConfig::Edit
    );
}

#[test]
fn openai_compatible_save_keeps_hand_written_profile_edit_tool() {
    for (original_name, name) in [(Some("old"), "old"), (Some("old"), "new"), (None, "old")] {
        // Given: 手書きの edit_tool を持つプロファイル
        let tmp = tempfile::tempdir().expect("temporary directory");
        let path = config_path(tmp.path());
        std::fs::write(&path, HAND_WRITTEN).expect("write config");

        // When: GUI の入力 (edit_tool を持たない) で保存・改名する
        save_openai_compatible_provider_edit(&path, &compatible_input(name), original_name)
            .expect("save succeeds");

        // Then: 再読込しても edit_tool が残る
        let config = Config::load_strict(&options(tmp.path())).expect("saved config loads");
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [name],
            "{original_name:?} -> {name}"
        );
        assert_eq!(
            config.providers[name].edit_tool,
            Some(EditToolConfig::ApplyPatch),
            "{original_name:?} -> {name}"
        );
    }
}

#[test]
fn codex_save_keeps_hand_written_profile_edit_tool() {
    for (original_name, name) in [(Some("work"), "work"), (Some("work"), "renamed")] {
        // Given: 手書きの edit_tool を持つ Codex プロファイル
        let tmp = tempfile::tempdir().expect("temporary directory");
        let path = config_path(tmp.path());
        save_codex_provider_edit(&path, &codex_input("work"), None).expect("initial save");
        let text = std::fs::read_to_string(&path).expect("read config");
        std::fs::write(&path, format!("{text}edit_tool = \"edit\"\n")).expect("hand edit");
        assert_eq!(
            Config::load_strict(&options(tmp.path()))
                .expect("hand edit loads")
                .providers["work"]
                .edit_tool,
            Some(EditToolConfig::Edit)
        );

        // When: GUI の入力で再保存・改名する
        save_codex_provider_edit(&path, &codex_input(name), original_name).expect("save");

        // Then: edit_tool が引き継がれる
        let config = Config::load_strict(&options(tmp.path())).expect("saved config loads");
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [name]
        );
        assert_eq!(
            config.providers[name].edit_tool,
            Some(EditToolConfig::Edit),
            "{original_name:?} -> {name}"
        );
    }
}

#[test]
fn saving_without_a_previous_edit_tool_writes_none() {
    // Given: edit_tool のない別名プロファイル
    let tmp = tempfile::tempdir().expect("temporary directory");
    let path = config_path(tmp.path());
    std::fs::write(&path, HAND_WRITTEN).expect("write config");

    // When: 新規プロファイルを保存する
    save_openai_compatible_provider_edit(&path, &compatible_input("fresh"), None).expect("save");

    // Then: 他プロファイルの edit_tool は流用されない
    let config = Config::load_strict(&options(tmp.path())).expect("saved config loads");
    assert_eq!(config.providers["fresh"].edit_tool, None);
    assert_eq!(
        config.providers["old"].edit_tool,
        Some(EditToolConfig::ApplyPatch)
    );
}

#[test]
fn model_entry_edit_tool_is_saved_as_a_table() {
    // Given: edit_tool だけを持つ有効なモデル
    let tmp = tempfile::tempdir().expect("temporary directory");
    let path = config_path(tmp.path());
    let model = ModelEntryConfig {
        edit_tool: Some(EditToolConfig::Edit),
        ..ModelEntryConfig::enabled("gpt-5")
    };
    let input = OpenAiCompatibleProviderInput {
        models: vec![model.clone(), ModelEntryConfig::enabled("gpt-6-astra")],
        ..compatible_input("compat")
    };

    // When: 保存する
    save_openai_compatible_provider_edit(&path, &input, None).expect("save");

    // Then: 文字列に短縮されず table として書かれ、再読込で一致する
    let text = std::fs::read_to_string(&path).expect("read config");
    assert!(
        text.contains(r#"{ id = "gpt-5", enabled = true, edit_tool = "edit" }"#),
        "{text}"
    );
    assert!(text.contains(r#""gpt-6-astra""#), "{text}");
    let config = Config::load_strict(&options(tmp.path())).expect("saved config loads");
    assert_eq!(config.providers["compat"].models[0], model);
}
