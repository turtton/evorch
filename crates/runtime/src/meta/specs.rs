use providers::ToolSpec;
use serde_json::{Value, json};

pub(crate) fn tool_spec(name: &str) -> ToolSpec {
    match name {
        "delegate" => ToolSpec {
            name: name.into(),
            description: "Delegate a task to a child agent. Role defaults to worker. By default, wait for the child and return its phase or an attention snapshot if it asks a question; use subagent_questions and answer_subagent_question to resolve that question. background=true returns immediately with a run_id. interactive=true requires background=true. Category is worker-only; images require multimodal_looker (alias: multimodallooker). Provide a self-contained prompt with purpose, file/responsibility ownership, constraints, expected outcome and validation. Ask for a final report covering outcome, changes, verification and unresolved issues. Let clear tasks finish independently; send intermediate messages only for blockers, scope/ownership changes or findings affecting other work.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": {"type": "string", "default": "worker", "enum": [
                        "orchestrator", "explorer", "worker", "reviewer", "planner", "oracle",
                        "multimodal_looker", "multimodallooker"
                    ]},
                    "prompt": {"type": "string", "description": "Self-contained task instructions and relevant context."},
                    "background": {"type": "boolean", "default": false, "description": "Return the child run_id immediately instead of waiting."},
                    "interactive": {"type": "boolean", "default": false, "description": "Keep the child available for messages; requires background=true."},
                    "name": {"type": "string", "description": "Human-readable child name."},
                    "category": {"type": "string", "enum": super::CATEGORIES, "description": "Worker-only task category for model routing."},
                    "workspace_mode": {"type": "string", "enum": ["shared", "isolated"], "default": "shared"},
                    "workspace_branch": {"type": "string", "description": "Existing branch for an isolated workspace."},
                    "load_skills": {"type": "array", "items": {"type": "string"}, "description": "Registered skills to load into the child."},
                    "task": {
                        "type": "object", "description": "Team-mode task assignment.",
                        "properties": {"id": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}}},
                        "required": ["id", "paths"], "additionalProperties": false
                    },
                    "images": {
                        "type": "array", "description": "Image payloads for multimodal_looker only.",
                        "items": {"type": "object", "properties": {
                            "media_type": {"type": "string"}, "data": {"type": "string", "description": "Base64-encoded image data."}
                        }, "required": ["media_type", "data"], "additionalProperties": false}
                    }
                },
                "required": ["prompt"]
            }),
        },
        "run_output" => ToolSpec {
            name: name.into(),
            description: "Retrieve a run's output without waiting or consuming its inbox. Only your direct children or parent are readable, like send_message; self, siblings and unrelated runs are denied. Returns phase and status (still_running, completed, cancelled, failed), output only on completion, and reason on failure/cancellation. Does not restore terminal runs.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"run_id": run_id()},
                "required": ["run_id"],
            }),
        },
        "wait" => {
            let mut spec = object_spec(name,
                "Wait for terminal completion of directly related parent/child runs using runtime notifications, without consuming their inboxes. Finish independent work first, then prefer the default 10-minute wait over repeated short waits or status polling. Supply exactly one of run_id or run_ids (1-8 unique IDs); mode=any returns when one finishes, mode=all when all finish. Either mode returns early when a target needs a required user answer, your inbox has an agent message to handle, or new user input arrives; automatic completion notifications alone still follow mode. Omit timeout_ms or use 600000 ms (10 minutes) for normal waiting; this is the maximum/default, and completion or attention returns immediately. timeout_ms=0 returns a snapshot. Timeout does not cancel targets. Returns {timed_out, inbox_ready, user_input_ready, completed_run_ids, attention_run_ids, runs}; timed_out is false on completion, required user input, an inbox message, or new user input. New user input has user_input_ready=true and is supplied automatically in the next model input, including images; do not use inbox to retrieve it. If inbox_ready is true, use inbox to read the queued messages, which wait does not consume. Each entry in runs includes phase/status/output/reason and truncation flags (output 4096 bytes, reason 1024 bytes). The legacy run_id-only call returns Done/Error upon completion, and a snapshot on timeout, required user input, an inbox message, or new user input. Self, sibling, unrelated and unknown IDs fail before waiting. Caller cancellation interrupts immediately.",
                json!({
                    "run_id": run_id(),
                    "run_ids": {"type":"array", "items":run_id(), "minItems":1, "maxItems":super::runs::waiting::MAX_WAIT_RUNS, "uniqueItems":true},
                    "mode":{"type":"string", "enum":["any","all"], "default":"any"},
                    "timeout_ms":{"type":"integer", "minimum":0, "maximum":super::runs::waiting::MAX_WAIT_MS, "default":super::runs::waiting::MAX_WAIT_MS}
                }), &[], true);
            spec.input_schema["oneOf"] = json!([
                {"required":["run_id"], "not":{"required":["run_ids"]}},
                {"required":["run_ids"], "not":{"required":["run_id"]}}
            ]);
            spec
        },
        "send_message" => object_spec(name,
            "Send a fire-and-forget message to a distinct direct child or parent. Returns message_id; does not wait for a reply. Unrelated, sibling and self recipients fail; absent or terminal recipients are restored only when the persisted restore contract permits delivery, otherwise the restore error is returned. Send only blockers, ownership/spec changes or findings that affect the recipient; avoid progress-only messages when a clear task can finish independently.",
            json!({"run_id":run_id(), "message":{"type":"string", "description":"Self-contained update or instruction."}}), &["run_id","message"], false),
        "send" => {
            let mut spec = object_spec(name,
                "Deliver a message to a distinct direct child or parent and return its message_id. kind defaults to send; reply requires reply_to containing the original message_id; steering provides a parent instruction at the recipient's next safe boundary. Unrelated, sibling and self recipients fail; absent or terminal recipients are restored only when the persisted restore contract permits delivery, otherwise the restore error is returned. Use wait_reply only when subsequent work depends on a reply.",
                json!({"run_id":run_id(), "message":{"type":"string"}, "kind":{"type":"string", "enum":["send","reply","steering"], "default":"send"}, "reply_to":{"type":"string", "description":"Original message_id, required for kind=reply."}}), &["run_id","message"], false);
            spec.input_schema["allOf"] = json!([{"if":{"properties":{"kind":{"const":"reply"}},"required":["kind"]},"then":{"required":["reply_to"]}}]);
            spec
        },
        "wait_reply" => object_spec(name,
            "Wait on mailbox notifications for a reply to a message previously sent by this run. message_id is the ID returned by send/send_message; timeout_ms is the maximum wait in milliseconds. Returns and consumes the matching reply without consuming unrelated messages. Timeout returns ReplyTimeout; recipient termination returns RunTerminated; unknown message IDs or invalid ownership fail. Prefer a substantial bounded wait over frequent inbox polling.",
            json!({"message_id":{"type":"string"},"timeout_ms":{"type":"integer","minimum":0}}), &["message_id","timeout_ms"], false),
        "inbox" => object_spec(name,
            "Drain and return this run's currently queued agent messages as a JSON array, including IDs, sender, kind, content and reply correlation. An empty array means no messages. This consumes those messages; use runtime completion notifications or wait/wait_reply instead of repeated inbox polling.",
            json!({}), &[], true),
        "skill_load" => object_spec(name,
            "Load the body of a registered skill, or one bundled resource relative to that skill's directory. Omit resource to read the skill body without frontmatter. Returns text; an unavailable registry, unknown skill, invalid resource path or read failure returns an error. Load a skill when the task needs it and follow referenced resources selectively.",
            json!({"name":{"type":"string","description":"Exact registered skill name."},"resource":{"type":"string","description":"Optional safe relative path to a bundled resource, for example references/guide.md."}}), &["name"], false),
        "cancel" => object_spec(name,
            "Request cooperative cancellation of a live run. Returns cancelled when the request is accepted; this does not mean termination has completed. Use wait to observe termination. An unknown run, ownership conflict or other runtime rejection returns an error.",
            json!({"run_id":run_id()}), &["run_id"], false),
        "list_agents" => object_spec(name,
            "Return registered runs with numeric run_id/parent_run_id, name, role_name, phase and model. IDs passed to other tools use the run-N string form. This is a current snapshot; use wait for completion instead of repeated list polling.",
            json!({}), &[], true),
        "inspect_agent" => object_spec(name,
            "Return a registered run's numeric run_id, role_name, phase, message_count and workspace metadata. An unknown run returns an error. This is an inspection snapshot; use wait for completion instead of polling.",
            json!({"run_id":run_id()}), &["run_id"], false),
        "compact" => object_spec(name,
            "Summarize this run's older conversation while preserving its task and unfinished work. Returns checkpoint_id, estimated_tokens_before/after, still_above_threshold and reason. A summary/model/storage failure returns an error; success may still leave context above threshold. This does not reset cumulative token or tool budgets.",
            json!({}), &[], true),
        "finish" => object_spec(name,
            "Submit the final result and end this run when its completion gate accepts it. Provide outcome, changed files/artifacts, validation performed and unresolved issues in the result. A rejected goal gate returns its reasons and next_action and leaves the run active; fix the reported conditions before retrying.",
            json!({"result":{"type":"string","description":"Self-contained completion report with outcome, changes, validation and unresolved issues."}}), &["result"], false),
        "escalate" => object_spec(name,
            "End a Direct run with a persisted handoff memo for an Orchestrator. Include the original request, concrete reason for escalation and enough findings/workspace state for independent continuation. Returns {escalated:true, source_run_id} after recording the memo. Missing/empty required text or unknown fields fails and leaves the run active; source_run_id is derived by runtime and must not be supplied.",
            json!({
                "original_request":{"type":"string","minLength":1},
                "escalation_reason":{"type":"string","minLength":1},
                "findings":strings(), "files_touched":strings(), "blockers":strings(),
                "workspace_state":{"type":"string"}, "suggested_next":{"type":"string"}
            }), &["original_request","escalation_reason"], true),
        "ledger_append" => object_spec(name,
            "Append a durable note to this run's ledger and return {seq}. Use concise facts needed after compaction/restart. A missing run store or rejected storage write returns a structured error; a failed append is not persisted.",
            json!({"body":{"type":"string","description":"Durable note text."}}), &["body"], true),
        "ledger_read" => object_spec(name,
            "Read this run's durable ledger as entries with seq, body and created_at_ns. Does not consume entries. A missing run store or failed read returns a structured error.",
            json!({}), &[], true),
        "submit_review" => object_spec(name,
            "Submit a typed reviewer verdict and acceptance-criterion evidence. Returns review submitted. verdict is approve or request-update; include actionable findings for requested changes. Each criterion includes id, status and note; evidence binds its command/exit_status to target_sha and optional artifacts. Malformed or unknown fields fail; this submission alone does not complete the run or satisfy a goal gate.",
            json!({
                "verdict":{"type":"string","enum":["approve","request-update"]},
                "findings":strings(),
                "criteria":{"type":"array","items":{
                    "type":"object", "additionalProperties":false,
                    "properties":{
                        "id":{"type":"string"},"status":{"type":"string","enum":["met","unmet","unknown"]},"note":{"type":"string"},
                        "evidence":{"type":["object","null"],"properties":{
                            "command":{"type":"string"},"exit_status":{"type":"integer","minimum":i32::MIN,"maximum":i32::MAX},"target_sha":{"type":"string"},
                            "diff_ref":{"type":["string","null"]},"artifact_path":{"type":["string","null"]},"red_evidence":{"type":["string","null"]}
                        },"required":["command","exit_status","target_sha"]}
                    },"required":["id","status","note"]
                }}
            }), &["verdict"], true),
        "ask_user" => object_spec(name,
            "Ask a focused scope question and return {question_id, status:pending, blocking, delivery} immediately. For a subagent this asks its Orchestrator parent; a root asks the user. Answers clarify the task and never grant execution permissions. title is required (1-2048 UTF-8 bytes); options is optional (up to 3 nonempty choices, each up to 256 UTF-8 bytes). blocking defaults to true: finish is refused until required answers arrive and are observed in a subsequent model turn. Continue independent work while the question is pending; answers are injected automatically, so do not poll for progress. Unknown arguments, invalid sizes, unavailable storage, more than 32 questions per recipient, or more than 1024 pending questions overall fail.",
            json!({"title":{"type":"string","minLength":1,"maxLength":2048},"options":{"type":"array","maxItems":3,"items":{"type":"string","minLength":1,"maxLength":256}},"blocking":{"type":"boolean","default":true}}), &["title"], true),
        "user_answers" => object_spec(name,
            "Return up to 32 questions created by this run or explicitly inherited from its resumed conversation, with pending/answered state and original requester provenance. This read does not consume answers. Answers are also injected automatically into the run, so use this for recovery or inspection rather than repeated polling. An unconfigured store returns an empty list; unknown arguments or durable storage read failures return an error.",
            json!({}), &[], true),
        "subagent_questions" => object_spec(name,
            "Read questions from one direct child run, including pending state and original requester. Use after wait reports attention for that child. Decide whether to answer from available context or ask the user on your own run. Only the direct Orchestrator parent may read these questions.",
            json!({"run_id":run_id()}), &["run_id"], true),
        "answer_subagent_question" => object_spec(name,
            "Answer one direct child's pending question. The answer is delivered to that child at its next model turn and wakes it from Waiting. Only the direct Orchestrator parent may answer; this never grants tool permissions. If user judgment is needed, ask_user on your own run first, then pass that answer here.",
            json!({"question_id":{"type":"string"},"answer":{"type":"string","minLength":1,"maxLength":4096}}), &["question_id","answer"], true),
        _ => panic!("missing meta tool contract: {name}"),
    }
}

fn run_id() -> Value {
    json!({"type":"string", "pattern":"^run-[0-9]+$", "description":"Runtime ID in run-N form."})
}

fn strings() -> Value {
    json!({"type":"array", "items":{"type":"string"}})
}

fn object_spec(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    strict: bool,
) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object", "properties":properties, "required":required, "additionalProperties":!strict}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_schema_exposes_optional_role_and_execution_flags() {
        // Given: the public delegate definition.
        let spec = tool_spec("delegate");
        // When: inspecting the machine-consumed schema.
        let properties = &spec.input_schema["properties"];
        // Then: prompt alone is required; both accepted multimodal aliases are advertised.
        assert_eq!(spec.input_schema["required"], serde_json::json!(["prompt"]));
        assert_eq!(
            properties["role"]["enum"],
            serde_json::json!([
                "orchestrator",
                "explorer",
                "worker",
                "reviewer",
                "planner",
                "oracle",
                "multimodal_looker",
                "multimodallooker"
            ])
        );
        assert_eq!(properties["role"]["default"], "worker");
        for name in ["background", "interactive"] {
            assert_eq!(properties[name]["type"], "boolean");
            assert_eq!(properties[name]["default"], false);
        }
        for name in [
            "prompt",
            "name",
            "category",
            "workspace_mode",
            "workspace_branch",
        ] {
            assert_eq!(properties[name]["type"], "string");
        }
        assert_eq!(properties["load_skills"]["type"], "array");
        assert_eq!(properties["task"]["type"], "object");
        assert_eq!(properties["images"]["type"], "array");
    }

    #[test]
    fn run_output_exposed_with_required_run_id_when_orchestrator() {
        // Given: the canonical registry and role capability filters.
        let specs = crate::META_OPS.iter().map(|name| tool_spec(name)).collect();
        // When: model-visible definitions are selected for the orchestrator.
        let visible =
            crate::ExecutionPolicy::for_role(agents::Role::Orchestrator).filter_tool_specs(specs);
        // Then: the callable tool includes its machine-consumed input contract.
        let spec = visible
            .iter()
            .find(|spec| spec.name == "run_output")
            .unwrap();
        assert_eq!(spec.input_schema["required"], serde_json::json!(["run_id"]));
        assert_eq!(spec.input_schema["properties"]["run_id"]["type"], "string");
    }
}

#[cfg(test)]
mod contracts;
