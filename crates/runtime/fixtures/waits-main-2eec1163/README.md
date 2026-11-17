# 旧 wait 保存形式 fixture（C14）

## 出自

- Producer: **2eec1163e225272b2722f4796d1522fce758bfa0**（変更前 main）。
- 専用 detached worktree: .worktrees/wait-fixtures-main-2eec1163。検証後、一時試験だけを除去してcleanを確認し、git worktree removeで削除済み。branch / commit は作成していない。main / feat/unify-wait の実装は編集していない。
- 計画: Planner run-01m7htt3zzf1mvttpzgs5dhymh #318.2 第5・7節、差分改訂 #337.2。制約: plan-review run-01m7hwbszn6qhgj2say3t1tvfy #173.1 の C14 と周辺。
- 本物の旧 AgentRuntime の wait / delegate / wait_reply handler を gated mock model から実行し、Waiting event → ToolsWaiting quiesce ACK → dispatch drain → capture_recovery_records / save_recovery_graph の順で採取した。新型で旧形式らしいJSONを生成したものではない。
- 一時試験以外は変更なし。production writer / schema / capability policy は元のまま。sleep・完了検知用timeout・実時間による合否判定なし。Reply の timeout_ms は採取対象の旧tool入力であって、試験の完了期限ではない。
- JSON はSQLiteの保存済み文字列bytesをそのまま保存。waits.jsonだけはconfig.json内の recovery_waits 配列の元のbyte range。再シリアライズ・整形・ID差し替え・deadline補正はしていない。
- SQLiteはgraph commit後のonline backup。元の保存JSONとの一致、integrity_check=okを確認した。無害なfixture本文とmock model入力のみを使用し、保存後のSecretRedactor検査でもcredential-shape検出なし。
- 編集前pullでmainはd7ef68f1へ進んだが、producer worktreeは指定SHAのまま。

## ファイル・parse型

各prefixに次の5種類がある。

| suffix | 元の保存データ / parse型 |
|---|---|
| .config.json | RunContextRecord.config_json → RunRestoreDescriptor。recovery_boundary=tools_waiting / interrupted_tool_calls=[] |
| .messages.json | RunContextRecord.messages_json → Vec<providers::Message>。元のToolUseを保持し、未完了callへのToolResultはまだない |
| .waits.json | config内のrecovery_waits配列のraw slice → Vec<DurableWait> |
| .member.json | recovery_members.record_json → RunRecoveryRecord (version=1)。recipe.descriptorとwaitsを含む |
| .intent.json | resume_intents.payload → ResumeIntent (version=1)、checkpoint.boundary=tools_waiting |

| prefix | requester | wait数 |
|---|---|---:|
| runs-legacy-true | run-01m7hzdmdxx0bj2vxm6en2a2n4 | 1 |
| runs-legacy-false | run-01m7hzdmfcrnsfy9ymr6ss8syn | 1 |
| delegates | run-01m7hzdmgg7xb7ngv2zd7dpp26 | 2 |
| reply | run-01m7hzdmj6cd0sffwnsym928dz | 1 |
| mixed | run-01m7hzdmhnxnf20w7g2mq8mm5j | 4 |

### 期待されるparse結果

- **runs-legacy-true**: wait(run_id=child)。mode=any / kind=Runs / legacy_result=true / consume_mailbox=true。call=wait-legacy、position=0。
- **runs-legacy-false**: wait(run_id=child, mode=any)。kind=Runs / legacy_result=false / consume_mailbox=true。run_idsではなくrun_idを使用し、明示modeでstructured結果になる旧契約を採取。call=wait-structured、position=0。
- **delegates**: foreground delegate2件の同一wave。wait2件、各mode=all / kind=Delegates / 同じcallsとtargets2件 / legacy_result=false / consume_mailbox=false。call=delegate-a,delegate-b、position=0,1。既存child IDを保持し、復元で再spawnしない。
- **reply**: 本物のWorkerが親に送ったmsg-1へのwait_reply中。kind=Reply / mode=any / legacy_result=false / consume_mailbox=true / delivered_answer_ids=[]。call=wait-reply、position=0。元入力timeout_ms=600000。旧writerのdeadline_unix_msは 1794826566168 のまま保存し、経過しても補正しない。
- **mixed**: Orchestratorの同じ未完了batchにDelegates2件（position0,1）とRuns legacy=true/false（position2,3）。実行中なのはforeground delegate waveで、後続Runsは旧writerが保存を認める未dispatchのobservation-only tail。単独Runs fixtureの2件では実際のwait handlerがWaitingに達している。

「単独形式」は各runのdescriptor / history / 待機証拠を指す。依存先のない1-member graphという意味ではない。

### 混在manifest

- replyのWorkerはmixedの直接子。同じ5-member component内に **Runs両legacy / Delegates / Replyの全形式**がある。
- mixedの親は子のrequestをモデル応答前にtake_inboxで読むため、delegate待機はこの本文で解除されない。msg-1の相関は保存済み。採取したmailboxに未読reply本文はないので、更新後の「未読を消費しない」試験では別途メッセージを投入すること。
- mixed-manifest.sqliteは **4 committed manifests / 計12 members**。componentサイズは2 / 2 / 3 / 5。5-member mixed componentはmixedのrequesterからrecovery_members.manifest_idを引いて特定する。他3 componentは単独形式の依存先込みgraph。
- mixed-manifest.edges.jsonは、その5-member mixed componentのrecovery_manifests.edges_jsonのraw bytes。
- 旧producerで新variantは作れない。計画の「旧／新混在」は更新後buildの試験で、この旧bytesを保持したまま新形式を追加・組み合わせる。このfixtureを新型で再生成しない。

### 更新後buildで守る結果（C14）

1. 全旧JSONをparseし、元call ID / 入力 / position / 参照を検証できる。追加fieldにはdefaultを設ける。
2. Runs両legacyとDelegatesは再開を維持し、Delegatesを再spawnしない。
3. Replyは元call IDへ廃止errorのToolResultを **1件だけ**追加して続行する。旧toolの再実行、mailbox消費、SentRecord解放をしない。旧consume_mailbox=trueというbytesを理由に退役時の消費をしない。
4. 二度目restoreで結果を重複しない。人によるContinue / 旧新混在 / 更新後provider継続の試験は実装担当側の範囲。

## 実行・検証

採取worktreeのrootをcwdにして実行し、完了までshell pollで待機した。

~~~sh
RUSTC_WRAPPER= WAIT_FIXTURE_OUT=/var/tmp/evo-1000-1b531f2b/thread-9gkyvvvf/wait-fixtures \
  /home/turtton/.ghr/github.com/turtton/evorch/.agents/skills/sandbox-build/scripts/with-dev-env.sh \
  cargo nextest run --offline -p runtime --lib capture_main_wait_fixtures --no-capture
~~~

- 生成: **1 passed**。12records / 4committed manifests、recovery_planは4component・blocked0。各形式のvalidate_recovery_waitsとRestoredState::from_recovery_bundle成功。
- 採取後、以下の独立再読試験を追加し、生成試験を除外して近傍試験とともに実行した。

~~~sh
RUSTC_WRAPPER= WAIT_FIXTURE_OUT=/var/tmp/evo-1000-1b531f2b/thread-9gkyvvvf/wait-fixtures \
  /home/turtton/.ghr/github.com/turtton/evorch/.agents/skills/sandbox-build/scripts/with-dev-env.sh \
  cargo nextest run --offline -p runtime --lib \
  -E '(test(agent_loop::tools_waiting::tests::) | test(restore::waiting::tests::)) & !test(capture_main_wait_fixtures)' --no-capture
~~~

- 結果: **14 passed**（独立再読1件＋既存近傍13件）。26JSONのparse、12memberの待機証拠、4manifestのcommitted状態、全5組の独立RunStore再読とvalidate、credential-shape検査が成功。再読はSQLiteのコピーに対して実施し、元fixtureを再生成・変更しない。
- 近傍既存試験はbatch順序 / cache prefix / observation / Reply相関 / Continue / 不正証拠の拒否を守る有価値な検証なので削除・変更しなかった。
- 初回wrapperなしbuildはLIBCLANG_PATH不足で失敗し、正規wrapperで解消。初期試作の子Orchestratorはroot-only境界で拒否されたため、policyを緩めずroot構成へ修正した。
- 新buildでのresume / Reply廃止挙動はここでは未検証。本作業は旧bytes採取と旧readerによる妥当性確認まで。
- ログ: 親ディレクトリのwait-fixture-generation.log / wait-fixture-validation.log。
- SHA256SUMSでpayload27個とREADMEを検証できる（このディレクトリでsha256sum -c SHA256SUMS）。

## Payload一覧

| file | bytes |
|---|---:|
| delegates.config.json | 2587 |
| delegates.intent.json | 44856 |
| delegates.member.json | 4238 |
| delegates.messages.json | 337 |
| delegates.waits.json | 1133 |
| mixed-manifest.edges.json | 761 |
| mixed-manifest.sqlite | 1789952 |
| mixed.config.json | 3388 |
| mixed.intent.json | 44852 |
| mixed.member.json | 5987 |
| mixed.messages.json | 560 |
| mixed.waits.json | 1869 |
| reply.config.json | 1578 |
| reply.intent.json | 14535 |
| reply.member.json | 2565 |
| reply.messages.json | 217 |
| reply.waits.json | 446 |
| runs-legacy-false.config.json | 1864 |
| runs-legacy-false.intent.json | 44867 |
| runs-legacy-false.member.json | 2760 |
| runs-legacy-false.messages.json | 227 |
| runs-legacy-false.waits.json | 378 |
| runs-legacy-true.config.json | 1846 |
| runs-legacy-true.intent.json | 44866 |
| runs-legacy-true.member.json | 2724 |
| runs-legacy-true.messages.json | 209 |
| runs-legacy-true.waits.json | 360 |

上表に加えてREADME.mdとSHA256SUMSがある。

## 生成に使ったコード（全文）

旧crates/runtime/src/agent_loop/tools_waiting/tests.rsに次のmodule宣言だけを追加した。

~~~rust
mod fixture_capture;
~~~

同ファイルのmodule基準のtests/fixture_capture.rsの採取時全文は以下。
super::*のcall / Role / Message / MessageRole / ContentBlock / jsonは指定SHAの既存tests.rsのimportとhelper（未変更）を使う。

~~~rust
//! Temporary producer harness; production writers are unmodified.
use super::*;
use crate::quiesce::{ProviderInput, RecoveryRecipe, ResumePolicy, RunRecoveryRecord};
use crate::{AgentInvocationContext, AgentModel, AgentRuntime, RunId, RunStore, RuntimeError};
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatRequest, ChatResponse, FinishReason, ToolSpec};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};
type Request = (RunId, oneshot::Sender<ChatResponse>);
struct GatedModel {
    requests: mpsc::UnboundedSender<Request>,
    inputs: Mutex<BTreeMap<String, ProviderInput>>,
}
#[async_trait::async_trait]
impl AgentModel for GatedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let input = ProviderInput {
            profile: "fixture".into(),
            protocol: model::ApiProtocol::OpenAiCompletions,
            edit_tool: config::EditToolConfig::Edit,
            affinity_key: invocation.run_id.clone(),
            replay: None,
            request: ChatRequest {
                model: "fixture-model".into(),
                messages: messages.to_vec(),
                tools: tools.to_vec(),
                tool_choice: None,
                temperature: None,
                max_tokens: None,
                reasoning_effort: None,
                service_tier: None,
                output_schema: None,
                observation: None,
                responses_replay: None,
            },
        };
        self.inputs
            .lock()
            .unwrap()
            .insert(invocation.run_id.clone(), input);
        let (tx, rx) = oneshot::channel();
        self.requests
            .send((invocation.run_id.parse().unwrap(), tx))
            .unwrap();
        Ok(rx.await.unwrap())
    }
    fn checkpoint_provider_input(&self, run: &str) -> Option<ProviderInput> {
        self.inputs.lock().unwrap().get(run).cloned()
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture-model".into()
    }
    fn requires_admission(&self) -> bool {
        false
    }
}
fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        usage: Default::default(),
        usage_reported: true,
        finish_reason,
    }
}
fn idle_response() -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: "fixture paused".into(),
        }],
        FinishReason::Stop,
    )
}
fn config() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    }
}
fn save(out: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(out.join(name), bytes).unwrap();
    assert_eq!(std::fs::read(out.join(name)).unwrap(), bytes);
}
// Extract the original JSON array's byte range, without serializing a parsed value.
fn raw_waits(json: &str) -> &str {
    let key = "\"recovery_waits\":";
    let tail = &json[json.find(key).unwrap() + key.len()..];
    let mut values = serde_json::Deserializer::from_str(tail).into_iter::<serde_json::Value>();
    assert!(values.next().unwrap().unwrap().is_array());
    &tail[..values.byte_offset()]
}
async fn waiting(events: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        let event = events.recv().await.unwrap();
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Waiting,
                ..
            }) if run_id == &run.to_string() => break,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error | AgentRunPhase::Stopped | AgentRunPhase::Done,
                ..
            }) if run_id == &run.to_string() => panic!("fixture wait terminated: {event:?}"),
            _ => {}
        }
    }
}
#[tokio::test]
async fn capture_main_wait_fixtures() {
    let out = std::path::PathBuf::from(std::env::var("WAIT_FIXTURE_OUT").unwrap());
    std::fs::create_dir_all(&out).unwrap();
    let dir = tempfile::tempdir_in(std::env::var("CARGO_MANIFEST_DIR").unwrap()).unwrap();
    let mut storage_config = storage::StorageConfig {
        db_path: dir.path().join("fixture.db"),
        ..Default::default()
    };
    storage_config.thread_index.enabled = false;
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut events = bus.subscribe();
    let (tx, mut requests) = mpsc::unbounded_channel();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(GatedModel {
            requests: tx,
            inputs: Mutex::default(),
        }),
    )
    .with_run_store(RunStore::open(&storage_config, storage.handle()).unwrap());
    let mut gates = Vec::new();
    let mut parents = Vec::new();
    for name in [
        "runs-legacy-true",
        "runs-legacy-false",
        "delegates",
        "mixed",
    ] {
        let parent = runtime.delegate_background(Role::Orchestrator, name.into(), config());
        let (id, parent_gate) = requests.recv().await.unwrap();
        assert_eq!(id, parent);
        let mut blocks = if name == "delegates" || name == "mixed" {
            vec![
                call(
                    "delegate-a",
                    "delegate",
                    json!({"target":{"role":"worker"},"prompt":"fixture child a"}),
                ),
                call(
                    "delegate-b",
                    "delegate",
                    json!({"target":{"role":"worker"},"prompt":"fixture child b"}),
                ),
            ]
        } else {
            let child = runtime
                .delegate_background_as_child(parent, Role::Worker, "fixture child", config())
                .unwrap();
            let (id, gate) = requests.recv().await.unwrap();
            assert_eq!(id, child);
            gates.push(gate);
            match name {
                "runs-legacy-true" => vec![call(
                    "wait-legacy",
                    "wait",
                    json!({"run_id":child.to_string()}),
                )],
                "runs-legacy-false" => vec![call(
                    "wait-structured",
                    "wait",
                    json!({"run_id":child.to_string(),"mode":"any"}),
                )],
                "reply" => {
                    let message = runtime
                        .send_agent_message(
                            parent,
                            child,
                            event_bus::AgentMessageKind::Send,
                            "fixture request",
                            None,
                        )
                        .unwrap();
                    vec![call(
                        "wait-reply",
                        "wait_reply",
                        json!({"message_id":message,"timeout_ms":600000}),
                    )]
                }
                _ => unreachable!(),
            }
        };
        if name == "mixed" {
            let blocker = runtime
                .delegate_background_as_child(parent, Role::Worker, "fixture blocker", config())
                .unwrap();
            let (id, gate) = requests.recv().await.unwrap();
            assert_eq!(id, blocker);
            gates.push(gate);
            let reply = runtime
                .delegate_background_as_child(
                    parent,
                    Role::Worker,
                    "fixture reply waiter",
                    config(),
                )
                .unwrap();
            let (id, gate) = requests.recv().await.unwrap();
            assert_eq!(id, reply);
            let message = runtime
                .send_agent_message(
                    reply,
                    parent,
                    event_bus::AgentMessageKind::Send,
                    "fixture request",
                    None,
                )
                .unwrap();
            assert_eq!(runtime.take_inbox(parent).unwrap().len(), 1);
            gate.send(response(
                vec![call(
                    "wait-reply",
                    "wait_reply",
                    json!({"message_id":message,"timeout_ms":600000}),
                )],
                FinishReason::ToolUse,
            ))
            .unwrap();
            waiting(&mut events, reply).await;
            parents.push(("reply", reply));
            blocks.push(call(
                "wait-legacy",
                "wait",
                json!({"run_id":blocker.to_string()}),
            ));
            blocks.push(call(
                "wait-structured",
                "wait",
                json!({"run_id":blocker.to_string(),"mode":"any"}),
            ));
        }
        parent_gate
            .send(response(blocks, FinishReason::ToolUse))
            .unwrap();
        if name == "delegates" || name == "mixed" {
            for _ in 0..2 {
                let (_, gate) = requests.recv().await.unwrap();
                gates.push(gate);
            }
        }
        waiting(&mut events, parent).await;
        parents.push((name, parent));
    }
    eprintln!("all real wait handlers entered Waiting (including the mixed component)");
    let mut changes = runtime.shared.quiesce.subscribe();
    runtime.request_quiesce();
    loop {
        let intents = runtime.resume_intents().unwrap();
        if parents.iter().all(|(_, id)| {
            let i = intents.iter().find(|i| i.run_id == *id).unwrap();
            runtime.shared.quiesce.acknowledged(*id, &i.incarnation)
        }) {
            break;
        }
        assert!(
            !matches!(
                runtime.quiesce_state(),
                crate::quiesce::QuiesceState::Failed { .. }
            ),
            "{:?}",
            runtime.quiesce_state()
        );
        changes.changed().await.unwrap();
    }
    eprintln!("all four ToolsWaiting snapshots acknowledged");
    // Only now release the master/child provider gates; parents are already parked.
    for gate in gates {
        gate.send(idle_response()).unwrap();
    }
    runtime.wait_quiesced().await.unwrap();
    let fence = runtime.fence_dispatch_restart().await.unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    let recipe = RecoveryRecipe {
        settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    };
    let recipes = runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .filter(|i| i.parent_run_id.is_none())
        .map(|i| (i.run_id, recipe.clone()))
        .collect();
    let records = runtime
        .capture_recovery_records(&runtime.fence_dispatch_restart().await.unwrap(), &recipes)
        .unwrap();
    assert_eq!(records.len(), 12);
    let manifests = runtime
        .save_recovery_graph("wait-fixture-main-2eec1163", &records)
        .unwrap();
    assert_eq!(manifests.len(), 4);
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 4, "{plan:?}");
    assert!(plan.report.blocked.is_empty(), "{plan:?}");
    assert_eq!(
        plan.components.iter().map(|c| c.runs.len()).sum::<usize>(),
        12
    );
    let store = runtime.shared.run_store.get().unwrap();
    let sql = rusqlite::Connection::open(&storage_config.db_path).unwrap();
    for (name, parent) in &parents {
        let bundle = store.restore_bundle(*parent).unwrap().unwrap();
        let descriptor: crate::restore::RunRestoreDescriptor =
            serde_json::from_str(&bundle.record.config_json).unwrap();
        crate::restore::validate_recovery_waits(&bundle.record, &descriptor.recovery_waits)
            .unwrap();
        let restored = crate::restore::RestoredState::from_recovery_bundle(
            &bundle,
            &descriptor.recovery_waits,
        )
        .unwrap();
        assert_eq!(restored.recovery_waits, descriptor.recovery_waits);
        let waits = &descriptor.recovery_waits;
        assert_eq!(
            waits.len(),
            match *name {
                "delegates" => 2,
                "mixed" => 4,
                _ => 1,
            }
        );
        for wait in waits {
            let is_delegate = matches!(
                wait.continuation.payload.as_ref().unwrap().kind,
                WaitKind::Delegates { .. }
            );
            assert_eq!(
                wait.continuation.legacy_result,
                *name == "runs-legacy-true"
                    || (*name == "mixed" && wait.tool_call_id == "wait-legacy")
            );
            assert_eq!(wait.continuation.consume_mailbox, !is_delegate);
            match &wait.continuation.payload.as_ref().unwrap().kind {
                WaitKind::Runs => assert!(name.starts_with("runs-") || *name == "mixed"),
                WaitKind::Delegates { calls } => {
                    assert!(*name == "delegates" || *name == "mixed");
                    assert_eq!(calls.len(), 2);
                }
                WaitKind::Reply {
                    message_id,
                    deadline_unix_ms,
                    ..
                } => {
                    assert_eq!(*name, "reply");
                    assert!(!message_id.is_empty());
                    assert_ne!(*deadline_unix_ms, 0);
                }
            }
        }
        save(
            &out,
            &format!("{name}.config.json"),
            bundle.record.config_json.as_bytes(),
        );
        save(
            &out,
            &format!("{name}.messages.json"),
            bundle.record.messages_json.as_bytes(),
        );
        save(
            &out,
            &format!("{name}.waits.json"),
            raw_waits(&bundle.record.config_json).as_bytes(),
        );
        let member: String = sql
            .query_row(
                "SELECT record_json FROM recovery_members WHERE run_id=?1",
                [parent.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: RunRecoveryRecord = serde_json::from_str(&member).unwrap();
        assert_eq!(parsed.waits, *waits);
        save(&out, &format!("{name}.member.json"), member.as_bytes());
        let intent: String = sql
            .query_row(
                "SELECT payload FROM resume_intents WHERE run_id=?1",
                [parent.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        save(&out, &format!("{name}.intent.json"), intent.as_bytes());
    }
    let mixed_run = parents.iter().find(|(name, _)| *name == "mixed").unwrap().1;
    let edges: String = sql.query_row("SELECT edges_json FROM recovery_manifests WHERE manifest_id=(SELECT manifest_id FROM recovery_members WHERE run_id=?1)", [mixed_run.to_string()], |row| row.get(0)).unwrap();
    save(&out, "mixed-manifest.edges.json", edges.as_bytes());
    // Backup copies committed SQLite pages; stored JSON payload bytes are unchanged.
    let mut dest = rusqlite::Connection::open(out.join("mixed-manifest.sqlite")).unwrap();
    let backup = rusqlite::backup::Backup::new(&sql, &mut dest).unwrap();
    assert_eq!(backup.step(-1).unwrap(), rusqlite::backup::StepResult::Done);
    drop(backup);
    let check: String = dest
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    for (name, parent) in &parents {
        let text: String = dest
            .query_row(
                "SELECT record_json FROM recovery_members WHERE run_id=?1",
                [parent.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            std::fs::read(out.join(format!("{name}.member.json"))).unwrap(),
            text.as_bytes()
        );
    }
    eprintln!(
        "saved four validated wait forms plus a committed mixed component (12 members in 4 manifests)"
    );
    // Test teardown drops parked tasks without resuming or mutating the captured state.
}
~~~

## 採取後の独立再読コード（全文）

同じ一時moduleに以下を追加して検証した。

~~~rust

#[test]
fn verify_saved_wait_fixtures() {
    let out = std::path::PathBuf::from(std::env::var("WAIT_FIXTURE_OUT").unwrap());
    let dir = tempfile::tempdir_in(std::env::var("CARGO_MANIFEST_DIR").unwrap()).unwrap();
    let copy = dir.path().join("fixture-copy.db");
    std::fs::copy(out.join("mixed-manifest.sqlite"), &copy).unwrap();
    let mut cfg = storage::StorageConfig {
        db_path: copy,
        ..Default::default()
    };
    cfg.thread_index.enabled = false;
    let storage = storage::Storage::open(cfg.clone()).unwrap();
    let store = RunStore::open(&cfg, storage.handle()).unwrap();
    let redactor = secret_guard::SecretRedactor::from_env();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&out).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                redactor.detect(&text).is_none(),
                "credential shape in {}",
                path.display()
            );
            serde_json::from_str::<serde_json::Value>(&text).unwrap();
            scanned += 1;
        }
    }
    assert_eq!(scanned, 26);
    let db = storage::Database::open(&cfg).unwrap();
    let loads = db.recovery_manifests().unwrap();
    assert_eq!(loads.len(), 4);
    let mut members = 0;
    for load in &loads {
        let manifest = load.record.as_ref().unwrap();
        assert!(manifest.committed && !manifest.superseded);
        assert_eq!(manifest.schema_version, 1);
        members += manifest.members.len();
        for member in &manifest.members {
            assert!(redactor.detect(&member.record_json).is_none());
            let record: RunRecoveryRecord = serde_json::from_str(&member.record_json).unwrap();
            let snapshot = db.run_context(&member.run_id).unwrap().unwrap();
            assert!(redactor.detect(&snapshot.config_json).is_none());
            assert!(redactor.detect(&snapshot.messages_json).is_none());
            let intent = db.resume_intent(&member.run_id).unwrap().unwrap();
            assert!(redactor.detect(&intent.payload).is_none());
            crate::restore::validate_recovery_waits(&snapshot, &record.waits).unwrap();
        }
    }
    assert_eq!(members, 12);
    for name in [
        "runs-legacy-true",
        "runs-legacy-false",
        "delegates",
        "reply",
        "mixed",
    ] {
        let waits_text = std::fs::read_to_string(out.join(format!("{name}.waits.json"))).unwrap();
        let waits: Vec<crate::quiesce::DurableWait> = serde_json::from_str(&waits_text).unwrap();
        let bundle = store.restore_bundle(waits[0].requester).unwrap().unwrap();
        assert_eq!(
            std::fs::read(out.join(format!("{name}.config.json"))).unwrap(),
            bundle.record.config_json.as_bytes()
        );
        assert_eq!(
            std::fs::read(out.join(format!("{name}.messages.json"))).unwrap(),
            bundle.record.messages_json.as_bytes()
        );
        assert_eq!(waits_text, raw_waits(&bundle.record.config_json));
        crate::restore::validate_recovery_waits(&bundle.record, &waits).unwrap();
        let restored =
            crate::restore::RestoredState::from_recovery_bundle(&bundle, &waits).unwrap();
        assert_eq!(restored.recovery_waits, waits);
        let member_text = std::fs::read_to_string(out.join(format!("{name}.member.json"))).unwrap();
        let member: RunRecoveryRecord = serde_json::from_str(&member_text).unwrap();
        assert_eq!(member.waits, waits);
        let intent_text = std::fs::read_to_string(out.join(format!("{name}.intent.json"))).unwrap();
        let intent: crate::quiesce::ResumeIntent = serde_json::from_str(&intent_text).unwrap();
        assert_eq!(intent.run_id, waits[0].requester);
        assert_eq!(
            intent.checkpoint.unwrap().boundary,
            crate::checkpoint::CheckpointBoundary::ToolsWaiting
        );
    }
    eprintln!(
        "independent reload: 26 JSON files, 12 members, 4 committed manifests; no credential shapes detected"
    );
}
~~~
