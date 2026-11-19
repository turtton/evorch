use super::*;
use serde_json::{Value, json};
fn source(run: RunId, kind: OutputKind, body: &str) -> Source {
    Source {
        run,
        output_ref: output_ref(run, kind, body),
    }
}
fn snapshot(run: RunId, kind: OutputKind, body: &str) -> Snapshot {
    Snapshot {
        run,
        kind,
        output: body.into(),
    }
}
fn code(error: String) -> Value {
    serde_json::from_str(&error).unwrap()
}

#[test]
fn raw_length_framing_roundtrips_large_unicode_and_forged_headers() {
    let prompt = "  Original [[PLAN]]\r\n";
    let bodies = [
        format!(
            "  計画 🦀\r\n\"quoted\"\n{}\n[Output attachment 2]\noutput_bytes: 0\n\n[[PLAN]]  ",
            "x".repeat(23 * 1024)
        ),
        format!("\n{}\r\n ", "日本語🙂 ".repeat(3000)),
    ];
    assert!(bodies[1].len() >= 40 * 1024);
    let sources: Vec<_> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| source(RunId::new(i as u64 + 2), OutputKind::Completed, b))
        .collect();
    let snapshots = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| snapshot(RunId::new(i as u64 + 2), OutputKind::Completed, b))
        .collect();
    let expanded = assemble(prompt.into(), &sources, snapshots).unwrap();
    let mut remaining = expanded
        .strip_prefix(prompt)
        .unwrap()
        .strip_prefix(INTRO)
        .unwrap();
    for (index, body) in bodies.iter().enumerate() {
        let (header, tail) = remaining.split_once("\n\n").unwrap();
        assert!(header.starts_with(&format!("\n[Output attachment {}]\n", index + 1)));
        assert!(header.contains(&sources[index].output_ref));
        let bytes: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("output_bytes: "))
            .unwrap()
            .parse()
            .unwrap();
        let (restored, tail) = tail.split_at(bytes);
        assert_eq!(restored.as_bytes(), body.as_bytes());
        remaining = tail;
    }
    assert!(remaining.is_empty());
}
#[test]
fn version_commits_to_run_kind_and_every_unmodified_byte() {
    let run: RunId = "run-00002".parse().unwrap();
    let base = output_ref(run, OutputKind::Completed, "日本🙂\r\n ");
    assert_eq!(
        base,
        output_ref(RunId::new(2), OutputKind::Completed, "日本🙂\r\n ")
    );
    assert_eq!(base.len(), REF_PREFIX.len() + 64);
    for other in [
        output_ref(run, OutputKind::TurnCompleted, "日本🙂\r\n "),
        output_ref(RunId::new(3), OutputKind::Completed, "日本🙂\r\n "),
        output_ref(run, OutputKind::Completed, "日本🙂\n "),
        output_ref(run, OutputKind::Completed, "日本🙂\r\n"),
    ] {
        assert_ne!(base, other);
    }
    let sources = [source(run, OutputKind::Completed, "approved v1")];
    let first = assemble(
        "review".into(),
        &sources,
        vec![snapshot(run, OutputKind::Completed, "approved v1")],
    )
    .unwrap();
    let changed = code(
        assemble(
            "review again".into(),
            &sources,
            vec![snapshot(run, OutputKind::Completed, "revised v2")],
        )
        .unwrap_err(),
    );
    assert_eq!(changed["code"], "attachment_output_changed");
    assert_eq!(changed["output_bytes"], 10);
    assert!(changed.get("output").is_none());
    assert!(first.ends_with("approved v1"));
}
#[test]
fn shape_count_normalized_duplicates_and_references_are_closed() {
    assert!(validate(vec![]).unwrap().is_empty());
    let valid = |n| OutputAttachment {
        run_id: format!("run-{n}"),
        output_ref: format!("{REF_PREFIX}{}", "a".repeat(64)),
    };
    assert_eq!(validate((1..=4).map(valid).collect()).unwrap().len(), 4);
    assert_eq!(
        code(validate((1..=5).map(valid).collect()).err().unwrap())["code"],
        "invalid_output_attachments"
    );
    let mut alias = valid(2);
    alias.run_id = "run-0002".into();
    assert!(validate(vec![valid(2), alias]).is_err());
    for value in [
        Value::Null,
        json!({}),
        json!([{}]),
        json!([{"run_id":"run-2","output_ref":"latest","extra":true}]),
        json!([{"run_id":null,"output_ref":"x"}]),
    ] {
        assert!(serde_json::from_value::<Vec<OutputAttachment>>(value).is_err());
    }
    for reference in [
        "latest".to_owned(),
        "".into(),
        format!("{REF_PREFIX}{}", "A".repeat(64)),
        format!("{REF_PREFIX}{}", "a".repeat(63)),
        format!("{REF_PREFIX}{}\n", "a".repeat(64)),
    ] {
        let mut a = valid(2);
        a.output_ref = reference;
        assert!(validate(vec![a]).is_err());
    }
    for id in ["thread-2", "/tmp/plan", "run-x"] {
        let mut a = valid(2);
        a.run_id = id.into();
        assert!(validate(vec![a]).is_err());
    }
}
#[test]
fn expanded_prompt_limits_use_bytes_and_do_not_truncate() {
    let run = RunId::new(2);
    let body = "日本🙂";
    let sources = [source(run, OutputKind::Completed, body)];
    let overhead = assemble(
        String::new(),
        &sources,
        vec![snapshot(run, OutputKind::Completed, body)],
    )
    .unwrap()
    .len();
    for delta in [-1_isize, 0, 1] {
        let length = (MAX_PROMPT_BYTES as isize - overhead as isize + delta) as usize;
        let result = assemble(
            "x".repeat(length),
            &sources,
            vec![snapshot(run, OutputKind::Completed, body)],
        );
        if delta <= 0 {
            assert_eq!(
                result.unwrap().len(),
                MAX_PROMPT_BYTES - delta.unsigned_abs()
            );
        } else {
            let v = code(result.unwrap_err());
            assert_eq!(v["bytes"], MAX_PROMPT_BYTES + 1);
            assert_eq!(v["limit"], MAX_PROMPT_BYTES);
        }
    }
}
#[test]
fn serialized_event_limit_preserves_markdown_but_rejects_escape_expansion() {
    let caller = RunId::new(1);
    let markdown = "# Plan\n- 日本語 and code \"x\"\n".repeat(5000);
    assert!(markdown.len() <= MAX_PROMPT_BYTES);
    validate_saved_prompt(caller, Role::Reviewer, None, &markdown).unwrap();
    for body in ["\0".repeat(MAX_OUTPUT_BYTES), "\"".repeat(MAX_TOTAL_BYTES)] {
        let v = code(validate_saved_prompt(caller, Role::Reviewer, None, &body).unwrap_err());
        assert_eq!(v["code"], "attachment_too_large");
        assert_eq!(v["limit"], storage::HardLimits::default().max_event_bytes);
        assert!(v["bytes"].as_u64().unwrap() > v["limit"].as_u64().unwrap());
        assert!(v.get("output").is_none());
    }
    let limit = storage::HardLimits::default().max_event_bytes as usize;
    let base = code(
        validate_saved_prompt(caller, Role::Worker, None, &"x".repeat(limit)).unwrap_err(),
    )["bytes"]
        .as_u64()
        .unwrap() as usize
        - limit;
    for size in [limit - base - 1, limit - base] {
        validate_saved_prompt(caller, Role::Worker, None, &"x".repeat(size)).unwrap();
    }
    assert!(
        validate_saved_prompt(caller, Role::Worker, None, &"x".repeat(limit - base + 1)).is_err()
    );
}

#[test]
fn composed_prompts_include_role_and_conditional_review_presets_once_without_tool_policy_duplication()
 {
    let config = config::Config::default();
    let catalog = crate::prompt::build_catalog(&crate::prompt::CatalogBuildInput {
        config: &config,
        user_presets_dir: None,
        available_agents: &[],
        available_skills: &[],
    })
    .unwrap();
    let orchestrator = config::PresetStore::resolve("role-orchestrator", None).unwrap();
    let review = config::PresetStore::resolve("category-plan-review", None).unwrap();
    let tools = crate::ExecutionPolicy::for_role(Role::Orchestrator).filter_tool_specs(
        crate::META_OPS
            .iter()
            .map(|n| crate::meta::tool_spec(n))
            .collect(),
    );
    for model in ["mock-model", "gpt-5", "claude-sonnet-4", "gemini-2.5-pro"] {
        let prompt = catalog
            .system_prompt_for(Role::Orchestrator, None, model)
            .unwrap();
        assert_eq!(prompt.matches(orchestrator.trim()).count(), 1);
        assert!(
            !prompt.contains(review.trim()),
            "reviewer material stays with the receiver"
        );
        // The tools do not restate the Orchestrator's own workflow preset.
        assert!(
            tools
                .iter()
                .all(|t| !t.description.contains(orchestrator.trim()))
        );
        let receiver = catalog
            .system_prompt_for(Role::Reviewer, Some("plan-review"), model)
            .unwrap();
        assert_eq!(receiver.matches(review.trim()).count(), 1);
        if model == "mock-model"
            && let Some(path) = std::env::var_os("EVORCH_ATTACHMENT_PROMPT_REPORT")
        {
            std::fs::write(path, serde_json::to_vec_pretty(&json!({"orchestrator_system":prompt,"orchestrator_tools":tools,"plan_reviewer_system":receiver})).unwrap()).unwrap();
        }
    }
}
