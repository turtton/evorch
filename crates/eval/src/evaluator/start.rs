use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

use super::groups::{self, Launch, Purpose};
use super::{Shared, parse, thread_cost};
use crate::cost::Cost;
use crate::metrics;
use crate::report;
use crate::roles::{self, SwappedRole};
use crate::runner;
use crate::source::{ChatThread, OriginalThread};

const ESTIMATE_NOTE: &str = "an estimate: each thread's original cost, repriced at current prices, times its replays (repeat × baseline and candidate); the simulated user's and the judge's requests are not included, and a replay can cost more or less than the original";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    thread_id: String,
    candidate: String,
    #[serde(default)]
    regression_thread_ids: Vec<String>,
    #[serde(default)]
    repeat: Option<u32>,
    #[serde(default)]
    regression_repeat: Option<u32>,
}

pub(super) struct Planned {
    pub thread_id: String,
    pub purpose: Purpose,
    pub problem: String,
    pub repeat: u32,
    pub cost: Cost,
    pub(super) project_root: PathBuf,
}

impl Planned {
    pub(super) fn replays(&self) -> u32 {
        self.repeat * 2
    }

    pub(super) fn estimate(&self) -> Cost {
        let mut total = Cost::default();
        for _ in 0..self.replays() {
            total.add(&self.cost);
        }
        total
    }
}

pub(super) fn total(plan: &[Planned]) -> Cost {
    let mut total = Cost::default();
    for planned in plan {
        total.add(&planned.estimate());
    }
    total
}

pub(super) fn estimate_value(plan: &[Planned]) -> Value {
    json!({
        "threads": plan.iter().map(|planned| json!({
            "thread_id": planned.thread_id,
            "purpose": planned.purpose,
            "repeat": planned.repeat,
            "replays": planned.replays(),
            "original_cost": report::cost_value(&planned.cost),
            "estimate": report::cost_value(&planned.estimate()),
        })).collect::<Vec<_>>(),
        "total": report::cost_value(&total(plan)),
        "note": ESTIMATE_NOTE,
    })
}

fn usd(cost: &Cost) -> String {
    let mut text = report::usd(cost);
    if !cost.is_complete() {
        text.push_str(&format!(": {}", report::incomplete_reason(cost)));
    }
    text
}

fn models(refs: &[roles::ModelRef]) -> String {
    let models = super::models(refs);
    if models.is_empty() {
        "-".into()
    } else {
        models.join(", ")
    }
}

fn summary(plan: &[Planned], swapped: &[SwappedRole], overlay: &str) -> String {
    let mut text = String::from("Start an evaluation of a candidate configuration (ADR 0036).\n\n");
    for planned in plan {
        let sample = match planned.purpose {
            Purpose::Target => "Target sample",
            Purpose::Regression => "Regression sample",
        };
        text.push_str(&format!(
            "{sample} {}: {}\n  {} × baseline and candidate = {} replays; original {} → estimated {}\n",
            planned.thread_id,
            metrics::clip(&planned.problem, 200),
            planned.repeat,
            planned.replays(),
            usd(&planned.cost),
            usd(&planned.estimate()),
        ));
    }
    text.push_str(&format!(
        "\nEstimated total: {}\nThis is {ESTIMATE_NOTE}.\n\nRoles the candidate changes:\n",
        usd(&total(plan))
    ));
    for role in swapped {
        text.push_str(&format!(
            "  {}: {} → {}\n",
            role.role,
            models(&role.baseline),
            models(&role.candidate)
        ));
    }
    text.push_str(&format!(
        "\nCandidate overlay:\n{}\n\nEach thread runs as a background `evorch eval run`, which stops if the GUI exits.",
        overlay.trim_end()
    ));
    text
}

pub(super) struct StartEvaluation(pub(super) Arc<Shared>);

impl StartEvaluation {
    async fn plan(&self, args: &StartArgs) -> Result<Vec<Planned>, String> {
        let user = self.0.context.user_config_dir.clone();
        let config = runner::load_config(Some(&user), None, None)
            .map_err(|error| format!("cannot load the configuration: {error}"))?;
        let mut wanted = vec![(
            args.thread_id.clone(),
            Purpose::Target,
            args.repeat.unwrap_or(config.eval.repeat_target),
        )];
        for thread_id in &args.regression_thread_ids {
            if wanted.iter().any(|(known, ..)| known == thread_id) {
                return Err(format!("thread {thread_id} is listed more than once"));
            }
            wanted.push((
                thread_id.clone(),
                Purpose::Regression,
                args.regression_repeat
                    .unwrap_or(config.eval.repeat_regression),
            ));
        }
        if wanted.iter().any(|(.., repeat)| *repeat == 0) {
            return Err("every repeat must be at least 1".into());
        }
        let prices = self.0.prices().await?;
        self.0
            .read(move |database| {
                wanted
                    .into_iter()
                    .map(|(thread_id, purpose, repeat)| {
                        let sample = database
                            .eval_sample(&thread_id)
                            .map_err(super::storage_error)?
                            .ok_or_else(|| {
                                format!("thread {thread_id} is not an evaluation sample; pin it with eval_save_sample first")
                            })?;
                        let thread = OriginalThread::load(database, &thread_id).map_err(|error| {
                            format!("thread {thread_id} cannot be replayed: {error}")
                        })?;
                        let start = database
                            .thread_start_state(&thread_id)
                            .map_err(super::storage_error)?
                            .ok_or_else(|| format!("thread {thread_id} has no recorded start state"))?;
                        let chat = ChatThread::load(database, &thread_id, &start.root_run_id).ok();
                        let cost = thread_cost(database, chat.as_ref(), &start.root_run_id, &prices)?;
                        Ok(Planned {
                            thread_id,
                            purpose,
                            problem: sample.problem,
                            repeat,
                            cost,
                            project_root: thread.project_root,
                        })
                    })
                    .collect()
            })
            .await
    }
}

#[async_trait::async_trait]
impl Tool for StartEvaluation {
    fn name(&self) -> &'static str {
        "eval_start"
    }

    fn description(&self) -> &'static str {
        "Start an evaluation of a candidate overlay on a pinned sample, optionally with other samples as regression checks. The user is shown the samples, replay counts, overlay and a cost estimate and must approve; the call waits for that answer. Each sample then replays in the background, and a notice arrives when all have ended. Defaults: repeat eval.repeat_target for the sample, eval.repeat_regression for regression samples."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": {"type": "string", "minLength": 1, "description": "The pinned sample the evaluation is about."},
                "candidate": {"type": "string", "minLength": 1, "description": "The candidate overlay as TOML, merged over the user's configuration. With a role_profile active, change role_profiles.<name> rather than agents or routing; eval, tools and comment_checker cannot change."},
                "regression_thread_ids": {"type": "array", "items": {"type": "string", "minLength": 1}, "description": "Other pinned samples the candidate must not make worse."},
                "repeat": {"type": "integer", "minimum": 1},
                "regression_repeat": {"type": "integer", "minimum": 1},
            },
            "required": ["thread_id", "candidate"],
            "additionalProperties": false,
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::process()
    }

    async fn execute(&self, _args: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error("eval_start needs the calling run"))
    }

    async fn execute_with_context(
        &self,
        ctx: &ToolExecutionContext,
        args: Value,
    ) -> Result<ToolResult, ToolError> {
        let args: StartArgs = parse(args)?;
        let user = self.0.context.user_config_dir.clone();
        let overlay = match runner::parse_overlay(&args.candidate, Some(&user)) {
            Ok(overlay) => overlay,
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "the candidate overlay is unusable: {error}"
                )));
            }
        };
        let plan = match self.plan(&args).await {
            Ok(plan) => plan,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let project = Some(plan[0].project_root.as_path());
        let swapped = match (
            runner::load_config(Some(&user), project, None),
            runner::load_config(Some(&user), project, Some(&overlay)),
        ) {
            (Ok(baseline), Ok(candidate)) => roles::swapped_roles(&baseline, &candidate),
            (Err(error), _) | (_, Err(error)) => {
                return Ok(ToolResult::error(format!(
                    "the candidate overlay is unusable in {}: {error}",
                    plan[0].project_root.display()
                )));
            }
        };
        if swapped.is_empty() {
            return Ok(ToolResult::error(format!(
                "the overlay changes no role for thread {}'s project, so both configurations would be the same; with a role_profile active, change role_profiles.<name>",
                plan[0].thread_id
            )));
        }
        let estimate = estimate_value(&plan);
        let request = summary(&plan, &swapped, &args.candidate);
        match super::ask(&self.0, ctx, "eval_start", request).await {
            Ok(()) => {}
            Err(declined) => {
                return Ok(ToolResult::error(format!(
                    "{declined}; nothing was started"
                )));
            }
        }
        let launches = plan
            .into_iter()
            .map(|planned| Launch {
                thread_id: planned.thread_id,
                purpose: planned.purpose,
                repeat: planned.repeat,
                problem: planned.problem,
            })
            .collect();
        let record = match groups::launch(&self.0, &ctx.run_id, &args.candidate, launches) {
            Ok(record) => record,
            Err(message) => {
                return Ok(ToolResult::error(format!(
                    "the evaluation could not start: {message}"
                )));
            }
        };
        let evaluations: Vec<Value> = record
            .members
            .iter()
            .map(|member| {
                json!({
                    "thread_id": member.thread_id,
                    "purpose": member.purpose,
                    "repeat": member.repeat,
                    "dir": member.dir,
                    "pid": member.pid,
                    "error": member.exit.as_ref().and_then(|exit| exit.error.clone()),
                })
            })
            .collect();
        Ok(ToolResult::success(
            json!({
                "started": true,
                "group": record.id,
                "evaluations": evaluations,
                "swapped_roles": swapped.iter().map(|role| json!({
                    "role": role.role,
                    "baseline": super::models(&role.baseline),
                    "candidate": super::models(&role.candidate),
                })).collect::<Vec<_>>(),
                "estimate": estimate,
                "note": "The evaluations run in the background and a notice arrives here when every one has ended; until then eval_report with this group shows their progress.",
            })
            .to_string(),
        ))
    }
}
