use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutionContext, ToolResult};

use super::groups::{self, Launch, Purpose};
use super::{Shared, parse, thread_cost};
use crate::changes::{self, Changes};
use crate::cost::{Cost, PriceTable};
use crate::metrics;
use crate::pins::{self, Overlay};
use crate::report;
use crate::roles::{self, SwappedRole};
use crate::runner;
use crate::source::{ChatThread, OriginalThread};
use config::{Config, EvalBudgetConfig};

const ESTIMATE_NOTE: &str = "an estimate: each thread's original cost, repriced at current prices, times its replays (repeat × baseline and candidate); the simulated user's and the judge's requests are not included, and a replay can cost more or less than the original";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    thread_id: String,
    candidate: String,
    #[serde(default)]
    common: Option<String>,
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
    /// The role the thread's replays start as.
    pub(super) role: runtime::Role,
    /// The sample's content hash when it was planned.
    pub(super) sample_hash: String,
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

/// What a comparison's cost presentation is computed from.
pub(super) struct Bounds<'a> {
    pub plan: &'a [Planned],
    /// The models the replays' agents run on, both sides.
    pub models: Vec<roles::ModelRef>,
    /// The dearest price per million tokens among `models`, or why it is unknown.
    pub dearest: Result<f64, String>,
    pub budget: &'a EvalBudgetConfig,
    pub simulator: Vec<roles::ModelRef>,
    pub judge: Vec<roles::ModelRef>,
}

/// The models a comparison runs its agents on: the role each thread starts as
/// and every role the changes reach, in the baseline and in the candidate.
pub(super) fn models_under_evaluation(
    plan: &[Planned],
    changes: &Changes,
    baseline: &Config,
    candidate: &Config,
) -> Vec<roles::ModelRef> {
    let mut addresses: Vec<String> = changes.affected_roles.clone();
    addresses.extend(
        plan.iter()
            .filter_map(|planned| roles::address(planned.role.name()))
            .map(str::to_owned),
    );
    addresses.sort();
    addresses.dedup();
    let mut found: Vec<roles::ModelRef> = addresses
        .iter()
        .flat_map(|address| {
            [baseline, candidate]
                .into_iter()
                .flat_map(|config| roles::role_models(config, address))
        })
        .collect();
    found.sort_by(|a, b| (&a.profile, &a.model).cmp(&(&b.profile, &b.model)));
    found.dedup();
    found
}

/// The dearest per-million-token price among `models`; unknown when any model
/// has no price or there is none.
pub(super) fn dearest_price(table: &PriceTable, models: &[roles::ModelRef]) -> Result<f64, String> {
    if models.is_empty() {
        return Err("no model could be resolved".into());
    }
    models.iter().try_fold(0.0_f64, |dearest, model| {
        table
            .ceiling_per_million(model)
            .map(|price| dearest.max(price))
            .ok_or_else(|| format!("{}/{} has no known price", model.profile, model.model))
    })
}

fn dollars(amount: f64) -> String {
    format!("${amount:.2}")
}

/// How much an approval commits to: the number of replays, the models, what
/// the budget converts to in money, what is billed outside it, and when the
/// evaluation stops. The conversion is the budget's token limit at the dearest
/// price, not a limit on spending, and the text says so.
pub(super) fn bounds_text(bounds: &Bounds) -> String {
    let Bounds {
        plan,
        models: agents,
        dearest,
        budget,
        simulator,
        judge,
    } = bounds;
    let replays: u32 = plan.iter().map(Planned::replays).sum();
    let pairs: u32 = plan.iter().map(|planned| planned.repeat).sum();
    let mut text = format!(
        "Runs: {replays} replays ({} threads × baseline and candidate × repeat), each replaying \
         the whole thread with a simulated user, then {pairs} judged pairs.\n",
        plan.len()
    );
    text.push_str(&format!("Models the agents run on: {}\n", models(agents)));
    text.push_str(&format!(
        "Budget per replay, over its whole delegation tree: {} tokens, {} tool calls, {} user turns, {} s.\n",
        budget.max_tokens, budget.max_tool_calls, budget.max_user_turns, budget.max_elapsed_secs
    ));
    match dearest {
        Ok(price) => {
            let per_replay = budget.max_tokens as f64 * price / 1_000_000.0;
            text.push_str(&format!(
                "That token budget priced at the dearest of those models ({}/M tokens): up to {} \
                 per replay, {} for all {replays} replays. It is a conversion of the budget, not \
                 a spending limit: the budget is checked after requests complete, so a request \
                 in flight or parallel sub-runs can pass it, and the rate is the highest price \
                 per token, not what a typical request pays.\n",
                dollars(*price),
                dollars(per_replay),
                dollars(per_replay * f64::from(replays)),
            ));
        }
        Err(reason) => text.push_str(&format!(
            "That token budget cannot be converted to money: {reason}.\n"
        )),
    }
    text.push_str(&format!(
        "Billed separately, in neither figure above: the simulated user ({}; one request per user \
         turn, up to {} per replay) and the judge ({}; about 3 requests per pair, so about {} in \
         all, more when one is retried).\n",
        models(simulator),
        budget.max_user_turns,
        models(judge),
        pairs * 3,
    ));
    text.push_str(
        "Stops: a replay ends as incomplete when it reaches any budget limit above; once a stop \
         is requested no further replay starts; the evaluations also end if the GUI exits.\n",
    );
    text
}

fn summary(
    plan: &[Planned],
    swapped: &[SwappedRole],
    changes: &Changes,
    common: Option<&str>,
    bounds: &str,
    digests: &[String],
    overlay: &str,
) -> String {
    let mut text = String::from("Start an evaluation of a candidate configuration (ADR 0036).\n\n");
    for (planned, digest) in plan.iter().zip(digests) {
        let sample = match planned.purpose {
            Purpose::Target => "Target sample",
            Purpose::Regression => "Regression sample",
        };
        text.push_str(&format!(
            "{sample} {}: {}\n  {} × baseline and candidate = {} replays; original {} → estimated {}\n  pinned: sample {}, inputs {}\n",
            planned.thread_id,
            metrics::clip(&planned.problem, 200),
            planned.repeat,
            planned.replays(),
            usd(&planned.cost),
            usd(&planned.estimate()),
            &planned.sample_hash[..planned.sample_hash.len().min(12)],
            &digest[..digest.len().min(12)],
        ));
    }
    text.push_str(&format!(
        "\nEstimated total: {}\nThis is {ESTIMATE_NOTE}.\n\nCost and limits:\n{bounds}\nRoles the candidate changes:\n",
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
    if !changes.tool_descriptions.is_empty() {
        text.push_str("Tool descriptions the candidate changes:\n");
        for change in &changes.tool_descriptions {
            text.push_str(&format!(
                "  {} for {}/{}\n",
                change.tool, change.profile, change.route_model_id
            ));
        }
    }
    text.push_str(&format!(
        "Roles the changes can reach: {}\n",
        changes.affected_roles.join(", ")
    ));
    if let Some(common) = common {
        text.push_str(&format!(
            "\nCommon overlay (both baseline and candidate):\n{}\n",
            common.trim_end()
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
    /// What each planned thread's evaluation would be approved on, hashed from
    /// the sample, overlays, configuration and presets as they are now.
    async fn digests(
        &self,
        plan: &[Planned],
        common: Option<&Overlay>,
        candidate: &Overlay,
    ) -> Result<Vec<String>, String> {
        let user = self.0.context.user_config_dir.clone();
        let thread_ids: Vec<String> = plan
            .iter()
            .map(|planned| planned.thread_id.clone())
            .collect();
        let hashes = self
            .0
            .read(move |database| {
                thread_ids
                    .iter()
                    .map(|thread_id| {
                        database
                            .eval_sample(thread_id)
                            .map_err(super::storage_error)?
                            .map(|sample| sample.content_hash())
                            .ok_or_else(|| {
                                format!("thread {thread_id} is no longer an evaluation sample")
                            })
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .await?;
        plan.iter()
            .zip(hashes)
            .map(|(planned, hash)| {
                pins::collect(
                    Some(&user),
                    &planned.project_root,
                    Some(hash),
                    common,
                    Some(candidate),
                )
                .map(|inputs| inputs.digest())
                .map_err(|error| format!("cannot pin the inputs of {}: {error}", planned.thread_id))
            })
            .collect()
    }

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
                        let sample_hash = sample.content_hash();
                        Ok(Planned {
                            thread_id,
                            purpose,
                            problem: sample.problem,
                            repeat,
                            cost,
                            project_root: thread.project_root,
                            role: thread.role,
                            sample_hash,
                        })
                    })
                    .collect()
            })
            .await
    }
}

/// Runs a launch on a blocking thread (each child waits for the process guard's
/// handshake, so it must hold no async worker) with the producer permit moved
/// into it. A caller that drops this future does not cancel the launch: it still
/// starts and registers its evaluations, so the permit must stay held until it
/// returns, or a restart could see the producers drained and then have an
/// evaluation appear.
pub(super) async fn launch_holding<T: Send + 'static>(
    producer: runtime::producer::ProducerPermit,
    launch: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(move || {
        let launched = launch();
        drop(producer);
        launched
    })
    .await
    .unwrap_or_else(|error| Err(error.to_string()))
}

#[async_trait::async_trait]
impl Tool for StartEvaluation {
    fn fixed_host_description(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "eval_start"
    }

    fn description(&self) -> &'static str {
        "Start an evaluation of a candidate overlay on a pinned sample, optionally with other samples as regression checks. The user is shown the samples, replay counts, models, overlays, a cost estimate with the budget's conversion to money and what is billed outside it, and the stop conditions, and must approve; the call waits for that answer, and the evaluation runs only on the inputs that were shown. Each sample then replays in the background, and a notice arrives when all have ended. Defaults: repeat eval.repeat_target for the sample, eval.repeat_regression for regression samples."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "thread_id": {"type": "string", "minLength": 1, "description": "The pinned sample the evaluation is about."},
                "candidate": {"type": "string", "minLength": 1, "description": "The candidate overlay as TOML, merged over the user's configuration and the common overlay. With a role_profile active, change role_profiles.<name> rather than agents or routing; eval, tools and comment_checker cannot change. A candidate must change something both sides do not share, and cannot change a role's model together with a tool description."},
                "common": {"type": "string", "minLength": 1, "description": "An overlay both the baseline and the candidate run under, as TOML: only agents, routing, role_profile and role_profiles. Use it to fix the model under evaluation on both sides."},
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
        if !self.0.lifecycle.producers.is_open() {
            return Ok(ToolResult::error("evaluation host is busy or stopping"));
        }
        let args: StartArgs = parse(args)?;
        let user = self.0.context.user_config_dir.clone();
        let candidate = match runner::parse_overlay(&args.candidate, Some(&user)) {
            Ok(value) => Overlay {
                text: args.candidate.clone(),
                value,
            },
            Err(error) => {
                return Ok(ToolResult::error(format!(
                    "the candidate overlay is unusable: {error}"
                )));
            }
        };
        let common = match args.common.as_deref().map(|text| {
            runner::parse_common(text, Some(&user)).map(|value| Overlay {
                text: text.to_owned(),
                value,
            })
        }) {
            Some(Ok(overlay)) => Some(overlay),
            Some(Err(error)) => {
                return Ok(ToolResult::error(format!(
                    "the common overlay is unusable: {error}"
                )));
            }
            None => None,
        };
        let plan = match self.plan(&args).await {
            Ok(plan) => plan,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let project = Some(plan[0].project_root.as_path());
        let (baseline_overlay, candidate_overlay) =
            pins::compose(common.as_ref(), Some(&candidate));
        let (baseline, candidate_config) = match (
            runner::load_config(Some(&user), project, baseline_overlay.as_ref()),
            runner::load_config(Some(&user), project, candidate_overlay.as_ref()),
        ) {
            (Ok(baseline), Ok(candidate)) => (baseline, candidate),
            (Err(error), _) | (_, Err(error)) => {
                return Ok(ToolResult::error(format!(
                    "the overlays are unusable in {}: {error}",
                    plan[0].project_root.display()
                )));
            }
        };
        let builtin = match runtime::tool_descriptions::BuiltinCatalog::for_evaluation() {
            Ok(builtin) => builtin,
            Err(error) => return Ok(ToolResult::error(error.to_string())),
        };
        let changes = match changes::accept(&baseline, &candidate_config, &builtin) {
            Ok(changes) => changes,
            Err(reason) => {
                return Ok(ToolResult::error(format!(
                    "the comparison is not valid for thread {}'s project: {reason}",
                    plan[0].thread_id
                )));
            }
        };
        let swapped = roles::swapped_roles(&baseline, &candidate_config);
        let prices = match self.0.prices().await {
            Ok(prices) => prices,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let agents = models_under_evaluation(&plan, &changes, &baseline, &candidate_config);
        let simulator = roles::role_models(&runner::simulator_config(&baseline).0, "worker");
        let judge = roles::role_models(&report::judge_config(&baseline).0, "orchestrator");
        let bounds = bounds_text(&Bounds {
            plan: &plan,
            dearest: dearest_price(&prices, &agents),
            models: agents,
            budget: &baseline.eval.budget,
            simulator,
            judge,
        });
        let digests = match self.digests(&plan, common.as_ref(), &candidate).await {
            Ok(digests) => digests,
            Err(message) => return Ok(ToolResult::error(message)),
        };
        let estimate = estimate_value(&plan);
        let request = summary(
            &plan,
            &swapped,
            &changes,
            args.common.as_deref(),
            &bounds,
            &digests,
            &args.candidate,
        );
        match super::ask(&self.0, ctx, "eval_start", request).await {
            Ok(()) => {}
            Err(declined) => {
                return Ok(ToolResult::error(format!(
                    "{declined}; nothing was started"
                )));
            }
        }
        // What was approved is what runs: the sample, overlays, configuration
        // and presets must still hash to what the user saw.
        match self.digests(&plan, common.as_ref(), &candidate).await {
            Ok(now) if now == digests => {}
            Ok(_) | Err(_) => {
                return Ok(ToolResult::error(
                    "the sample, overlays, configuration or prompt presets changed while the \
                     approval was pending; nothing was started. Call eval_start again to \
                     approve the current ones",
                ));
            }
        }
        let launches = plan
            .into_iter()
            .zip(digests)
            .map(|(planned, approved_digest)| Launch {
                thread_id: planned.thread_id,
                purpose: planned.purpose,
                repeat: planned.repeat,
                problem: planned.problem,
                approved_digest,
            })
            .collect();
        let Some(producer) = self.0.lifecycle.producers.try_enter() else {
            return Ok(ToolResult::error("evaluation host is busy or stopping"));
        };
        let shared = Arc::clone(&self.0);
        let (run_id, candidate, common) = (
            ctx.run_id.clone(),
            args.candidate.clone(),
            args.common.clone(),
        );
        let launched = launch_holding(producer, move || {
            groups::launch(&shared, &run_id, &candidate, common.as_deref(), launches)
        })
        .await;
        let record = match launched {
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
                "changes": changes,
                "estimate": estimate,
                "note": "The evaluations run in the background and a notice arrives here when every one has ended; until then eval_report with this group shows their progress.",
            })
            .to_string(),
        ))
    }
}
