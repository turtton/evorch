//! Opt-in, read-only model evaluation. Corpus commands are NEVER executed.
use super::*;
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf};

#[tokio::test]
#[ignore = "requires host credentials/network; see fixtures/shell-review/README.md"]
async fn live_shell_review_corpus() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let corpus_text = std::fs::read_to_string(root.join("fixtures/shell-review/corpus.json"))?;
    let corpus: Value = serde_json::from_str(&corpus_text)?;
    let output = PathBuf::from(std::env::var("EVORCH_REVIEW_EVAL_OUTPUT")?);
    let baseline: Option<Value> = std::env::var_os("EVORCH_REVIEW_EVAL_BASELINE")
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap());
    let instruction = baseline
        .as_ref()
        .map_or(REVIEW_INSTRUCTION, |b| b["instruction"].as_str().unwrap());
    let schema = baseline
        .as_ref()
        .map_or_else(shell_verdict_schema, |b| providers::JsonSchema {
            name: "sandbox_escalation_verdict".into(),
            schema: b["schema"].clone(),
        });
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    let user_dir = config::user_config_dir().ok_or("missing user config")?;
    let config = config::Config::load(&config::LoadOptions::default())?;
    let candidates = &config.routing.routes["command-review"];
    writeln!(
        file,
        "{}",
        json!({
            "protocol": corpus["protocol"],
            "corpus_sha256": sha256_hex(corpus_text.as_bytes()),
            "instruction_sha256": sha256_hex(instruction.as_bytes()),
            "schema": schema.schema,
        })
    )?;
    for profile in corpus["protocol"]["profiles"].as_array().unwrap() {
        let profile = profile.as_str().unwrap();
        let candidate = candidates
            .iter()
            .find(|c| c.profile == profile)
            .ok_or("profile drift")?;
        assert_eq!(candidate.model.as_deref(), Some("codex-auto-review"));
        let mut pinned = config.clone();
        pinned
            .routing
            .routes
            .retain(|name, _| name == "command-review");
        pinned.providers.retain(|name, _| name == profile);
        pinned
            .routing
            .routes
            .insert("command-review".into(), vec![candidate.clone()]);
        let model = crate::compose::compose_routed_model(
            &pinned,
            routing::ComposeDeps {
                credential_store: sandbox::credential::open_default(&user_dir)?,
                event_bus: None,
                env: Arc::new(routing::ProcessEnv),
                catalog: model::ModelCatalog::new(),
                factory: routing::factory::FactoryOptions::default(),
            },
            None,
        )?;
        let reviewer = QuickModelReviewer::new(Arc::new(ExplainFailure(model)));
        for case in corpus["cases"].as_array().unwrap() {
            for trial in 0..corpus["protocol"]["repetitions"].as_u64().unwrap() {
                let payload = &case["payload"];
                let context = &payload["context"];
                let procedure_source = context["project_rules"]["text"].as_str().is_some()
                    || context["loaded_skills"]
                        .as_array()
                        .is_some_and(|v| !v.is_empty());
                let mut audit = Audit::begin(
                    ReviewKind::Shell,
                    "host_unsandboxed",
                    Some("shell"),
                    "eval",
                    None,
                );
                let wire = reviewer
                    .request_verdict(
                        "eval",
                        CategoryId::ToolExecution,
                        Role::Reviewer,
                        (instruction.into(), schema.clone()),
                        payload,
                        &mut audit,
                    )
                    .await;
                let record = match wire {
                    Ok(wire) => {
                        let decision = if baseline.is_some() {
                            judge(&wire, "shell action", procedure_source)
                        } else {
                            judge_shell(
                                &wire,
                                procedure_source,
                                context["real_user_requests"].as_array().map_or(0, Vec::len),
                            )
                        };
                        json!({"approve": wire.approve, "reason": wire.reason,
                            "risk": wire.risk_level, "authorization": wire.authorization_level,
                            "procedure_step": wire.procedure_step, "authorization_basis": wire.authorization_basis, "authorization_refs": wire.authorization_refs, "scope_match": wire.scope_match, "decision": format!("{decision:?}")})
                    }
                    Err(error) => json!({"error": error.to_string()}),
                };
                writeln!(
                    file,
                    "{}",
                    json!({"id":case["id"],"group":case["group"],
                    "profile":profile,"model":"codex-auto-review","trial":trial,"result":record})
                )?;
                file.flush()?;
                eprintln!("{} {profile} {trial}: {record}", case["id"]);
                // A deterministic credential/transport failure is not a trial result.
                if record.get("error").is_some() {
                    return Err("evaluation unavailable; partial results retained".into());
                }
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "read-only access to historical host store; EVORCH_REVIEW_HISTORY_DB required"]
fn historical_review_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let db = rusqlite::Connection::open_with_flags(
        std::env::var("EVORCH_REVIEW_HISTORY_DB")?,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    // Bound the scan and output. Do not deserialize old manifests through the current version.
    let mut query = db.prepare(
        "SELECT id,payload FROM events WHERE kind='Diagnostic' ORDER BY id DESC LIMIT 5000",
    )?;
    let mut counts = std::collections::BTreeMap::new();
    for row in query.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })? {
        let (_, text) = row?;
        let value: Value = serde_json::from_str(&text)?;
        if text.contains("escalation_review_manifest") {
            fn find_detail(value: &Value) -> Option<&Value> {
                value
                    .get("detail")
                    .or_else(|| value.as_object()?.values().find_map(find_detail))
            }
            let detail = find_detail(&value);
            if let Some(detail) = detail.and_then(Value::as_str)
                && let Ok(manifest) = serde_json::from_str::<Value>(detail)
            {
                let key = format!(
                    "v{}:{}:{}",
                    manifest["version"],
                    manifest["kind"],
                    manifest["outcome"]["verdict"]["decision"]
                );
                *counts.entry(key).or_insert(0usize) += 1;
            }
        }
    }
    eprintln!("historical metadata (no user text): {counts:?}");
    Ok(())
}

struct ExplainFailure(Arc<dyn AgentModel>);
#[async_trait::async_trait]
impl AgentModel for ExplainFailure {
    fn selected_model(&self, role: Role, category: Option<&str>) -> String {
        self.0.selected_model(role, category)
    }
    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.0.complete(ctx, role, messages, tools).await
    }
    async fn complete_structured(
        &self,
        ctx: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        schema: &providers::JsonSchema,
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        let result = self
            .0
            .complete_structured(ctx, role, messages, schema)
            .await;
        if let Err(error) = &result {
            eprintln!("evaluation provider failure: {error}");
        }
        result
    }
}
