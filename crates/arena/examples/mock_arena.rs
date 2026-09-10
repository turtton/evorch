use arena::{ArenaConfig, ArenaSpec, Attribution, Runner, TaskSpec};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{ProviderAuth, provider::openai_compatible::OpenAiCompatibleClient};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("a", "a", ["2"]).with_usage(3, 1),
        ScriptedResponse::text_stream("b", "b", ["3"]).with_usage(3, 1),
    ]);
    let client =
        OpenAiCompatibleClient::new(server.base_url(), "local", Duration::from_secs(2), None)?;
    let dir = tempfile::tempdir()?;
    let store = storage::Storage::open(storage::StorageConfig {
        db_path: dir.path().join("arena.db"),
        ..Default::default()
    })?;
    let spec = ArenaSpec {
        id: "demo".into(),
        project: "demo".into(),
        task: TaskSpec {
            id: "arithmetic".into(),
            prompt: "1+1".into(),
            expected_output: "2".into(),
        },
        configs: ["a", "b"]
            .into_iter()
            .map(|id| ArenaConfig {
                id: id.into(),
                profile: "local".into(),
                model: id.into(),
                attribution: Attribution::Worker,
            })
            .collect(),
        max_output_tokens: 16,
        total_token_budget: 200,
        timeout_ms: 1000,
    };
    let runner = Runner {
        client: &client,
        auth: &ProviderAuth::new("test"),
        storage: store.handle(),
    };
    let report = arena::run(&spec, &runner).await?;
    println!("{}", serde_json::to_string_pretty(report.traces())?);
    println!(
        "Selected: {:?}; active routing unchanged",
        report.selected()
    );
    Ok(())
}
