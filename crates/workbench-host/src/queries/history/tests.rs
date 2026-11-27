use super::*;
use event_bus::LifecycleEvent;
use std::collections::BTreeMap;

#[derive(Default)]
struct LegacyScanner {
    positions: std::collections::BTreeMap<String, u64>,
    parts: std::collections::BTreeMap<(String, String), PendingParts>,
    initial_receipts: std::collections::BTreeSet<String>,
    first_prompt: std::collections::BTreeMap<String, Vec<([u8; 32], u64)>>,
    last_assistant: std::collections::BTreeMap<String, Vec<([u8; 32], u64)>>,
    starts: std::collections::BTreeMap<(String, String), Option<storage::history::ToolInput>>,
}
impl LegacyScanner {
    fn observe(&mut self, kind: &EventKind, tokenized: bool) -> Option<RawMessage> {
        let mut tool = None;
        let (run, role, parts) = if let EventKind::Message(MessageEvent::MessagePart {
            run_id,
            response_id,
            text,
            index,
            total,
        }) = kind
        {
            let key = (run_id.clone(), response_id.clone());
            if *index == 0 {
                self.parts.insert(key.clone(), (0, *total, vec![]));
            }
            let pending = self.parts.get_mut(&key)?;
            if pending.0 != *index || pending.1 != *total || *total == 0 || *total > 4096 {
                self.parts.remove(&key);
                return None;
            }
            pending.0 += 1;
            pending.2.push((text.clone(), tokenized));
            if pending.0 != *total {
                return None;
            }
            let parts = self.parts.remove(&key)?.2;
            (run_id.as_str(), MessageRole::Assistant, parts)
        } else {
            let mut start = None;
            if let Some((run, call, input)) = storage::history::rows::tool_start(kind, tokenized) {
                self.starts.insert((run.to_owned(), call.to_owned()), input);
            } else if let EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                call_id,
                run_id: Some(run),
                ..
            }) = kind
            {
                start = self
                    .starts
                    .remove(&(run.clone(), call_id.clone()))
                    .flatten();
            }
            let row = committed_row(kind, tokenized, start)?;
            tool = row.tool;
            (row.run, message_role(row.role), row.parts)
        };
        let prompt = matches!(
            kind,
            EventKind::Lifecycle(LifecycleEvent::TaskPromptPublished { .. })
        );
        self.message_with(
            run,
            role,
            parts,
            tool,
            prompt,
            matches!(
                kind,
                EventKind::Message(MessageEvent::FinalResultPublished { .. })
            ),
        )
    }
    fn input(&mut self, input: storage::host_input::DeliveredHostInput) -> Option<RawMessage> {
        if input.first_in_run {
            self.initial_receipts.insert(input.run_id.clone());
        }
        if let Some(value) = input.text {
            return self.message(
                &input.run_id,
                MessageRole::User,
                vec![(value, false)],
                false,
                false,
            );
        }
        let index = self.positions.entry(input.run_id.clone()).or_default();
        let raw = RawMessage {
            run: input.run_id,
            index: *index,
            role: MessageRole::User,
            parts: vec![],
            input: Some(InputBody {
                id: input.prompt_id,
                bytes: input.text_bytes,
            }),
            tool: None,
        };
        *index += 1;
        Some(raw)
    }
    fn message(
        &mut self,
        run: &str,
        role: MessageRole,
        parts: Vec<(String, bool)>,
        prompt: bool,
        finish: bool,
    ) -> Option<RawMessage> {
        self.message_with(run, role, parts, None, prompt, finish)
    }
    fn message_with(
        &mut self,
        run: &str,
        role: MessageRole,
        parts: Vec<(String, bool)>,
        tool: Option<storage::history::ToolRow>,
        prompt: bool,
        finish: bool,
    ) -> Option<RawMessage> {
        use sha2::{Digest, Sha256};
        let mut normalized: Vec<(String, bool)> = Vec::new();
        for (part, tokenized) in parts {
            let deferred = tokenized && storage::BodyRef::from_token(&part).is_some();
            if !deferred && let Some((last, false)) = normalized.last_mut() {
                last.push_str(&part);
            } else {
                normalized.push((part, deferred));
            }
        }
        let key: Vec<_> = normalized
            .iter()
            .map(|(part, deferred)| {
                if *deferred {
                    let r = storage::BodyRef::from_token(part).unwrap();
                    (r.hash, r.byte_len)
                } else {
                    (Sha256::digest(part.as_bytes()).into(), part.len() as u64)
                }
            })
            .collect();
        if (matches!(role, MessageRole::Assistant | MessageRole::Reasoning)
            && key.iter().all(|(_, n)| *n == 0))
            || (finish && self.last_assistant.get(run) == Some(&key))
        {
            return None;
        }
        if prompt {
            if self.first_prompt.get(run) == Some(&key) {
                return None;
            }
            if !self.first_prompt.contains_key(run) {
                self.first_prompt.insert(run.to_owned(), key.clone());
                // The first accepted receipt ID of this run, replayed before its
                // first task, establishes identity without comparing bodies.
                // Later changed instructions are not that initial submission.
                if self.initial_receipts.contains(run) {
                    return None;
                }
            }
        }
        if role == MessageRole::Assistant {
            self.last_assistant.insert(run.to_owned(), key);
        }
        let parts = normalized;
        let index = self.positions.entry(run.to_owned()).or_default();
        let raw = RawMessage {
            run: run.to_owned(),
            index: *index,
            role,
            parts,
            input: None,
            tool,
        };
        *index += 1;
        Some(raw)
    }
}

fn legacy_committed_messages(
    db: &storage::Database,
    runs: &[String],
    high_water: i64,
) -> Result<Vec<RawMessage>, storage::StorageError> {
    let mut inputs = Vec::new();
    for run in runs {
        inputs.extend(db.delivered_host_input_history(run)?);
    }
    inputs.sort_by(|a, b| (a.accepted_at_ns, &a.prompt_id).cmp(&(b.accepted_at_ns, &b.prompt_id)));
    let mut inputs = inputs.into_iter().peekable();
    let mut raw = Vec::new();
    let mut scanner = LegacyScanner::default();
    let mut failure = None;
    db.for_each_event_for_runs_deferred(runs, 0, high_water, |row| match row {
        storage::DeferredRow::Failed(_) => {
            failure = Some(storage::StorageError::Io(
                "unreadable committed history".into(),
            ))
        }
        storage::DeferredRow::Event(row) => {
            let at = match row
                .event
                .meta
                .wall_clock
                .duration_since(std::time::UNIX_EPOCH)
            {
                Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
                Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
            };
            // Match the GUI's durable replay: accepted user inputs precede the
            // first event at or after their acceptance timestamp (ID breaks ties).
            while inputs.peek().is_some_and(|r| r.accepted_at_ns <= at) {
                if let Some(message) = scanner.input(inputs.next().unwrap()) {
                    raw.push(message);
                }
            }
            if let Some(message) = scanner.observe(
                &row.event.kind,
                row.format == storage::StoredFormat::Tokenized,
            ) {
                raw.push(message);
            }
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    // A receipt is authoritative even if its notification has not been stored.
    for input in inputs {
        if let Some(message) = scanner.input(input) {
            raw.push(message);
        }
    }
    Ok(raw)
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

fn append(store: &storage::Storage, clock: i64, kind: impl Into<EventKind>) {
    let mut event = event_bus::Event::new(kind);
    event.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(clock as u64);
    store.handle().append_event(None, &event).unwrap();
}

fn receipt(
    store: &storage::Storage,
    run: &str,
    id: &str,
    text: &str,
    at: i64,
    status: storage::HostInputStatus,
) {
    store
        .handle()
        .append_host_input_accepted(
            &[],
            &[],
            None,
            &storage::HostInputReceipt {
                provenance: storage::HostInputProvenance::User,
                prompt_id: id.into(),
                run_id: run.into(),
                root_run_id: run.into(),
                text: text.into(),
                status,
                reason: None,
                accepted_at_ns: at,
                status_at_ns: at,
                evidence_id: None,
            },
        )
        .unwrap();
}

fn per_run(messages: &[RawMessage]) -> BTreeMap<String, Vec<serde_json::Value>> {
    let mut runs = BTreeMap::<_, Vec<_>>::new();
    for message in messages {
        runs.entry(message.run.clone())
            .or_default()
            .push(serde_json::to_value(message).unwrap());
    }
    runs
}

#[test]
fn fixed_digest_matches_vector_oracle_at_parts_boundaries() {
    use sha2::{Digest, Sha256};
    fn token(index: usize, len: u64) -> (String, bool) {
        let hash = Sha256::digest(index.to_be_bytes());
        let text = format!("\u{e000}evorch-body:v1:{hash:x}:{len}");
        assert!(storage::BodyRef::from_token(&text).is_some());
        (text, true)
    }
    for count in [0, 1, 2, 4096] {
        let base: Vec<_> = (0..count).map(|i| token(i, 1)).collect();
        let mut reversed = base.clone();
        reversed.reverse();
        let mut longer = base.clone();
        if let Some(first) = longer.first_mut() {
            *first = token(0, 2);
        }
        let mut extra = base.clone();
        extra.push(token(0, 0));
        let cases = [
            base.clone(),
            reversed,
            longer,
            extra,
            vec![token(0, 0); count],
            vec![("a".into(), false), ("bc".into(), false)],
            vec![("abc".into(), false)],
        ];
        let mut old = LegacyScanner::default();
        let mut new = storage::history::Reducer::default();
        for parts in cases {
            for (role, prompt, finish) in [
                (MessageRole::Assistant, false, false),
                (MessageRole::Assistant, false, true),
                (MessageRole::User, true, false),
                (MessageRole::User, true, false),
            ] {
                let expected = old.message("run", role, parts.clone(), prompt, finish);
                let actual = new
                    .message(storage_role(role), parts.clone(), prompt, finish)
                    .map(|m| RawMessage {
                        run: "run".into(),
                        index: m.index,
                        role,
                        parts: m.parts,
                        input: None,
                        tool: None,
                    });
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    serde_json::to_value(expected).unwrap(),
                    "parts={count}, role={role:?}, finish={finish}"
                );
                assert!(serde_json::to_vec(&new).unwrap().len() < 512);
            }
        }
        new.next = u64::MAX;
        assert!(serde_json::to_vec(&new).unwrap().len() < 512);
    }
}

#[test]
fn initial_input_identity_uses_acceptance_sequence_not_delivery_or_clock_order() {
    use storage::HostInputStatus::*;
    for status in [Accepted, Withdrawn, Unknown, Delivered] {
        for first_at in [9, 10, 11] {
            let dir = tempfile::tempdir().unwrap();
            let config = storage::StorageConfig {
                db_path: dir.path().join("host.db"),
                ..Default::default()
            };
            let store = storage::Storage::open(config.clone()).unwrap();
            receipt(&store, "run", "first", "initial", first_at, status);
            // Earlier clock, but later acceptance sequence: never initial.
            receipt(&store, "run", "followup", "later", 8, Delivered);
            let prompt = || LifecycleEvent::TaskPromptPublished {
                run_id: "run".into(),
                agent_name: "worker".into(),
                role: "worker".into(),
                parent_run_id: None,
                prompt: "task".into(),
                user_text: None,
                skills: vec![],
            };
            append(&store, 10, prompt());
            append(&store, 12, prompt());
            append(
                &store,
                13,
                MessageEvent::MessageCompleted {
                    run_id: "run".into(),
                    text: "answer".into(),
                },
            );
            let db = storage::Database::open(&config).unwrap();
            // H before f: only receipts, even when their clock is in the future.
            let before = committed_messages(&db, &["run".into()], 0).unwrap();
            assert_eq!(before.len(), 1 + usize::from(status == Delivered));
            let raw = committed_messages(&db, &["run".into()], i64::MAX).unwrap();
            let mut expected = vec!["later"];
            if status == Delivered && first_at <= 10 {
                expected.push("initial");
            } else {
                expected.push("task");
                if status == Delivered {
                    expected.push("initial");
                }
            }
            expected.push("answer");
            assert_eq!(
                raw.iter()
                    .map(|m| m.parts[0].0.as_str())
                    .collect::<Vec<_>>(),
                expected,
                "{status:?}, first_at={first_at}"
            );
            assert_eq!(
                raw.iter().map(|m| m.index).collect::<Vec<_>>(),
                (0..expected.len() as u64).collect::<Vec<_>>()
            );
        }
    }
}

fn seed_history(store: &storage::Storage, runs: &[String], rng: &mut Rng) {
    use storage::HostInputStatus::*;
    let large = "q".repeat(20 * 1024);
    let mut late = Vec::new();
    for i in 0..rng.below(9) {
        let run = rng.pick(runs);
        let id = format!("{:03}-{i}", rng.below(1000));
        let text = *rng.pick(&["task", "more", "task", large.as_str()]);
        let at = 990 + rng.below(140) as i64;
        let status = *rng.pick(&[
            Delivered, Delivered, Delivered, Accepted, Unknown, Withdrawn,
        ]);
        if status == Accepted && rng.below(2) == 0 {
            late.push(storage::HostInputTransition {
                prompt_id: id.clone(),
                status: Delivered,
                reason: None,
            });
        }
        receipt(store, run, &id, text, at, status);
    }
    let mut clock = 1_000;
    for n in 0..40 {
        clock += rng.below(4) as i64;
        let run = rng.pick(runs).clone();
        match rng.below(8) {
            0 | 1 => {
                let text = rng.pick(&["x", "y", "", "task"]).to_string();
                append(
                    store,
                    clock,
                    MessageEvent::MessageCompleted { run_id: run, text },
                );
            }
            2 => {
                let text = rng.pick(&["x", "y", ""]).to_string();
                append(
                    store,
                    clock,
                    MessageEvent::FinalResultPublished { run_id: run, text },
                );
            }
            3 => {
                let total = 1 + rng.below(3) as usize;
                let mut index = 0;
                while index < total {
                    append(
                        store,
                        clock,
                        MessageEvent::MessagePart {
                            run_id: run.clone(),
                            response_id: format!("r{n}"),
                            text: rng.pick(&["p", "", "q"]).to_string(),
                            index,
                            total,
                        },
                    );
                    clock += rng.below(2) as i64;
                    index = if rng.below(5) == 0 { 0 } else { index + 1 };
                }
            }
            4 => {
                let prompt = rng.pick(&["task", "other"]).to_string();
                let user_text = (rng.below(2) == 0).then(|| prompt.clone());
                append(
                    store,
                    clock,
                    LifecycleEvent::TaskPromptPublished {
                        run_id: run,
                        agent_name: "Worker".into(),
                        role: "Worker".into(),
                        parent_run_id: None,
                        prompt,
                        user_text,
                        skills: vec![],
                    },
                );
            }
            5 => {
                if rng.below(3) != 0 {
                    append(
                        store,
                        clock,
                        event_bus::ToolEvent::ToolStarted {
                            tool_name: "t".into(),
                            call_id: format!("c{n}"),
                            input: Some(serde_json::json!({"command": format!("run {n}")})),
                            run_id: Some(run.clone()),
                            parent_call_id: None,
                        },
                    );
                }
                append(
                    store,
                    clock,
                    event_bus::ToolEvent::ToolCompleted {
                        tool_name: "t".into(),
                        call_id: format!("c{n}"),
                        is_error: false,
                        output: (rng.below(3) != 0).then(|| "out".into()),
                        detail: None,
                        run_id: Some(run),
                        parent_call_id: None,
                    },
                );
            }
            6 => append(
                store,
                clock,
                MessageEvent::MessageDelta {
                    delta: "d".into(),
                    run_id: Some(run),
                },
            ),
            _ => append(
                store,
                clock,
                MessageEvent::MessageCompleted {
                    run_id: run,
                    text: large.clone(),
                },
            ),
        }
    }
    store.handle().transition_host_inputs(&late).unwrap();
}

#[test]
fn per_run_fold_matches_legacy_global_fold_under_monotonic_clocks() {
    let runs: Vec<String> = ["run-a", "run-b", "run-c"].map(String::from).into();
    let (mut merged, mut deferred_inputs, mut trailing) = (0, 0, 0);
    for seed in 0..48 {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("host.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        seed_history(&store, &runs, &mut Rng(seed));
        let db = storage::Database::open(&config).unwrap();
        let through = db.last_event_id().unwrap();
        let legacy = legacy_committed_messages(&db, &runs, through).unwrap();
        let history = committed_messages(&db, &runs, through).unwrap();
        let scanner = CommitScanner {
            cut: Some(0),
            ..Default::default()
        };
        let step = scanner.advance(&db, &runs).unwrap();
        assert!(!step.rewritten, "seed {seed}");
        let mut window = Vec::new();
        for (order, message) in step.fresh {
            trailing += usize::from(order.0 == i64::MAX);
            merged += usize::from(order.0 != i64::MAX && !order.1);
            window.push(message);
        }
        deferred_inputs += legacy.iter().filter(|m| m.input.is_some()).count();
        assert_eq!(per_run(&history), per_run(&legacy), "seed {seed}");
        assert_eq!(per_run(&window), per_run(&legacy), "seed {seed}");
    }
    assert!(merged > 0 && deferred_inputs > 0 && trailing > 0);
}
