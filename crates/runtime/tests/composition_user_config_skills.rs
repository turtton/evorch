//! `RuntimeComposition::user_config_dir` が skill の user スコープ探索にも
//! 一貫して使われることを確認する。
//!
//! グローバル既定の解決 (`XDG_CONFIG_HOME`) を環境変数で差し替えるため、
//! このバイナリには env を書き換える test を 1 つだけ置く。
//! 他の test と同一プロセスにならないので干渉しない。

mod support;

use std::path::Path;
use std::sync::Arc;

use config::Config;
use event_bus::EventBus;
use runtime::{AgentRunPhase, ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::DirectSandbox;
use sandbox::credential::FileCredentialStore;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

fn write_skill(user_config: &Path, name: &str) {
    let directory = user_config.join("skills").join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: fixture {name}\n---\nBody of {name}.\n"),
    )
    .expect("SKILL.md");
}

const PRESET: &str = "user-config-probe";

fn write_preset(user_config: &Path, body: &str) {
    let directory = user_config.join("presets");
    std::fs::create_dir_all(&directory).expect("presets directory");
    std::fs::write(directory.join(format!("{PRESET}.md")), body).expect("preset");
}

/// compose_runtime した runtime が実際にモデルへ渡した system prompt 全文を返す。
async fn system_prompt_for(user_config_dir: Option<&Path>) -> String {
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some(PRESET.to_owned());
    let bus = Arc::new(EventBus::new(32));
    let credentials = tempfile::tempdir().expect("credential directory");
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        providers::FinishReason::Stop,
    ))]));
    let composed = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: user_config_dir.map(Path::to_path_buf),
        config: &config,
        executor: Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
        bus,
        credential_store: Arc::new(
            FileCredentialStore::open(credentials.path()).expect("credential store"),
        ),
        env: Arc::new(routing::MapEnv::from_iter([("UNUSED", "unused")])),
        model_source: ModelSource::Fixed(model.clone()),
        workspace: None,
    })
    .expect("runtime composes");
    let run = composed.runtime.delegate_background(
        Role::Orchestrator,
        "skills".to_string(),
        RunConfig::default(),
    );
    assert_eq!(composed.runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    observed[0]
        .iter()
        .filter(|message| message.role == providers::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Given: グローバル user config (XDG_CONFIG_HOME) と明示指定の user config dir に別々の skill がある
// When: 明示指定ありと指定なしで compose_runtime する
// Then: 指定ありは指定側の skill と user presets だけを使い、グローバル側は見えない。
//       指定なしは従来どおりグローバル側を使う
#[tokio::test]
async fn explicit_user_config_dir_selects_user_scope_skills() {
    let sandbox = tempfile::tempdir().expect("scratch directory");
    let xdg = sandbox.path().join("xdg");
    let global = xdg.join("evorch");
    let selected = sandbox.path().join("selected");
    write_skill(&global, "global-only-skill");
    write_skill(&selected, "selected-only-skill");
    write_preset(&global, "GLOBAL-PRESET-BODY");
    write_preset(&selected, "SELECTED-PRESET-BODY");
    // SAFETY: このバイナリの唯一の test で、他スレッドは env を読み書きしない。
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &xdg);
        std::env::set_var("HOME", sandbox.path().join("home"));
    }

    let explicit = system_prompt_for(Some(&selected)).await;
    assert!(explicit.contains("- selected-only-skill: "), "{explicit}");
    assert!(!explicit.contains("- global-only-skill: "), "{explicit}");
    assert!(explicit.contains("SELECTED-PRESET-BODY"), "{explicit}");
    assert!(!explicit.contains("GLOBAL-PRESET-BODY"), "{explicit}");

    let fallback = system_prompt_for(None).await;
    assert!(fallback.contains("- global-only-skill: "), "{fallback}");
    assert!(!fallback.contains("- selected-only-skill: "), "{fallback}");
    assert!(fallback.contains("GLOBAL-PRESET-BODY"), "{fallback}");
    assert!(!fallback.contains("SELECTED-PRESET-BODY"), "{fallback}");
}
