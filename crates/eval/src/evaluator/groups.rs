//! Evaluations the Evaluator starts: one background `evorch eval run` per
//! thread, launched together as a group and watched until every process has
//! exited, then reported to the run that started them (ADR 0036 §1).
//!
//! A group lives in `<eval dir>/.groups/<id>/`: `group.json`, the candidate
//! overlay and each process's output. The processes end with the GUI process,
//! so a group a later GUI session finds unfinished was interrupted.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Shared, read_json};
use crate::output::{self, Manifest};
use crate::report::{self, Report};
use crate::runner::{BASELINE, CANDIDATE};

pub(super) const GROUPS_DIR: &str = ".groups";
const GROUP_FILE: &str = "group.json";
const OVERLAY_FILE: &str = "candidate.toml";
const CLI: &str = "evorch";
const TAIL_BYTES: u64 = 16 * 1024;
const TAIL_CHARS: usize = 1_200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Purpose {
    Target,
    Regression,
}

impl Purpose {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Regression => "regression",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Why the process could not be started or waited for.
    pub error: Option<String>,
    pub finished_at: String,
}

impl Exit {
    fn now(code: Option<i32>, signal: Option<i32>, error: Option<String>) -> Self {
        Self {
            code,
            signal,
            error,
            finished_at: output::timestamp(SystemTime::now()),
        }
    }

    fn of(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self::now(status.code(), status.signal(), None)
    }

    fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    fn describe(&self) -> String {
        match (&self.error, self.code, self.signal) {
            (Some(error), ..) => error.clone(),
            (None, Some(code), _) => format!("exit code {code}"),
            (None, None, Some(signal)) => format!("killed by signal {signal}"),
            (None, None, None) => "exited".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Member {
    pub thread_id: String,
    pub purpose: Purpose,
    pub repeat: u32,
    pub dir: PathBuf,
    pub pid: Option<u32>,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit: Option<Exit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct GroupRecord {
    pub id: String,
    /// The Evaluator run that started the group.
    pub run_id: String,
    pub started_at: String,
    pub candidate: PathBuf,
    pub members: Vec<Member>,
    pub finished_at: Option<String>,
    /// The run the completion notice reached, or why it reached none.
    pub notice: Option<String>,
}

impl GroupRecord {
    fn dir(&self) -> Option<&Path> {
        self.candidate.parent()
    }

    fn save(&self) {
        let Some(dir) = self.dir() else { return };
        if let Err(error) = output::write_json(&dir.join(GROUP_FILE), self) {
            tracing::warn!(%error, group = %self.id, "evaluation group record not saved");
        }
    }
}

/// Groups this process started, so their unfinished members are known to be running.
#[derive(Default)]
pub(super) struct Groups {
    live: Mutex<HashMap<String, Arc<Mutex<GroupRecord>>>>,
}

impl Groups {
    fn insert(&self, record: Arc<Mutex<GroupRecord>>) {
        let id = lock(&record).id.clone();
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, record);
    }

    fn is_live(&self, id: &str) -> bool {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(id)
    }

    pub(super) fn running_dirs(&self) -> HashSet<OsString> {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .flat_map(|record| {
                lock(record)
                    .members
                    .iter()
                    .filter(|member| member.exit.is_none())
                    .filter_map(|member| member.dir.file_name().map(OsStr::to_os_string))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `evorch` CLI next to `exe`, as Cargo and the Nix package install it
/// beside `evorch-gui`, or else the first one on `path`.
///
/// # Errors
/// Names where it looked when neither has one.
pub fn find_cli(exe: Option<&Path>, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let executable = |candidate: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    let sibling = exe.and_then(Path::parent).map(|dir| dir.join(CLI));
    if let Some(sibling) = sibling.as_deref().filter(|sibling| executable(sibling)) {
        return Ok(sibling.to_path_buf());
    }
    if let Some(found) = path
        .into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(CLI))
        .find(|candidate| executable(candidate))
    {
        return Ok(found);
    }
    Err(format!(
        "cannot find the `{CLI}` CLI{} or on PATH; install it together with the GUI",
        sibling.map_or_else(String::new, |sibling| format!(" at {}", sibling.display()))
    ))
}

pub(super) struct Launch {
    pub thread_id: String,
    pub purpose: Purpose,
    pub repeat: u32,
    pub problem: String,
}

fn unused_dir(base: PathBuf) -> PathBuf {
    if !base.exists() {
        return base;
    }
    let name = base
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    (2..)
        .map(|n| base.with_file_name(format!("{name}-{n}")))
        .find(|dir| !dir.exists())
        .unwrap_or(base)
}

fn group_id(now: SystemTime) -> Result<String, String> {
    let mut bytes = [0_u8; 3];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    let stamp = chrono::DateTime::<chrono::Utc>::from(now).format("%Y%m%d-%H%M%S");
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{stamp}-{suffix}"))
}

fn spawn(cli: &Path, args: &[OsString], member: &Member) -> std::io::Result<tokio::process::Child> {
    let mut command = tokio::process::Command::new(cli);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&member.stdout)?)
        .stderr(std::fs::File::create(&member.stderr)?);
    end_with_parent(&mut command);
    command.spawn()
}

/// Ask the kernel to stop the evaluation when this process ends, crashes
/// included. The signal follows the spawning thread, so callers spawn from
/// an async task on a runtime worker, which lives as long as the runtime.
#[cfg(target_os = "linux")]
fn end_with_parent(command: &mut tokio::process::Command) {
    let parent = rustix::process::getpid();
    // SAFETY: between fork and exec the closure makes only async-signal-safe
    // system calls and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))?;
            if rustix::process::getppid() != Some(parent) {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn end_with_parent(_command: &mut tokio::process::Command) {}

/// Start one `eval run` per thread and watch them; fails without a group when
/// no process could be started.
pub(super) fn launch(
    shared: &Arc<Shared>,
    run_id: &str,
    overlay: &str,
    launches: Vec<Launch>,
) -> Result<GroupRecord, String> {
    let context = &shared.context;
    let cli = match &context.cli {
        Some(cli) => cli.clone(),
        None => find_cli(
            std::env::current_exe().ok().as_deref(),
            std::env::var_os("PATH").as_deref(),
        )?,
    };
    let now = SystemTime::now();
    let id = group_id(now)?;
    let dir = context
        .user_config_dir
        .join("eval")
        .join(GROUPS_DIR)
        .join(&id);
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let candidate = dir.join(OVERLAY_FILE);
    std::fs::write(&candidate, overlay)
        .map_err(|error| format!("{}: {error}", candidate.display()))?;
    let mut members = Vec::new();
    let mut children = Vec::new();
    for (index, launch) in launches.into_iter().enumerate() {
        let mut member = Member {
            dir: unused_dir(output::default_dir(
                &context.user_config_dir,
                &launch.thread_id,
                now,
            )),
            thread_id: launch.thread_id,
            purpose: launch.purpose,
            repeat: launch.repeat,
            pid: None,
            stdout: dir.join(format!("{index}.stdout.log")),
            stderr: dir.join(format!("{index}.stderr.log")),
            exit: None,
        };
        let args: Vec<OsString> = [
            OsString::from("eval"),
            "run".into(),
            "--thread".into(),
            member.thread_id.clone().into(),
            "--candidate".into(),
            candidate.clone().into(),
            "--problem".into(),
            launch.problem.into(),
            "--repeat".into(),
            member.repeat.to_string().into(),
            "--output".into(),
            member.dir.clone().into(),
            "--db".into(),
            context.database.clone().into(),
            "--user-config".into(),
            context.user_config_dir.clone().into(),
        ]
        .into();
        match spawn(&cli, &args, &member) {
            Ok(child) => {
                member.pid = child.id();
                children.push((members.len(), child));
            }
            Err(error) => {
                member.exit = Some(Exit::now(
                    None,
                    None,
                    Some(format!("cannot start {}: {error}", cli.display())),
                ));
            }
        }
        members.push(member);
    }
    let mut record = GroupRecord {
        id,
        run_id: run_id.to_owned(),
        started_at: output::timestamp(now),
        candidate,
        members,
        finished_at: None,
        notice: None,
    };
    if children.is_empty() {
        record.finished_at = Some(output::timestamp(SystemTime::now()));
        record.save();
        return Err(record
            .members
            .iter()
            .filter_map(|member| member.exit.as_ref().map(Exit::describe))
            .collect::<Vec<_>>()
            .join("; "));
    }
    record.save();
    let snapshot = record.clone();
    let record = Arc::new(Mutex::new(record));
    shared.groups.insert(Arc::clone(&record));
    for (index, child) in children {
        tokio::spawn(watch(Arc::clone(shared), Arc::clone(&record), index, child));
    }
    Ok(snapshot)
}

async fn watch(
    shared: Arc<Shared>,
    record: Arc<Mutex<GroupRecord>>,
    index: usize,
    mut child: tokio::process::Child,
) {
    let exit = match child.wait().await {
        Ok(status) => Exit::of(status),
        Err(error) => Exit::now(None, None, Some(format!("cannot wait for it: {error}"))),
    };
    let finished = {
        let mut record = lock(&record);
        if let Some(member) = record.members.get_mut(index) {
            member.exit = Some(exit);
        }
        if record.members.iter().all(|member| member.exit.is_some()) {
            record.finished_at = Some(output::timestamp(SystemTime::now()));
        }
        record.save();
        record.finished_at.is_some().then(|| record.clone())
    };
    let Some(finished) = finished else { return };
    let (notice, all_succeeded) = tokio::task::spawn_blocking(move || completion_notice(&finished))
        .await
        .unwrap_or_else(|error| (error.to_string(), false));
    let run_id = lock(&record).run_id.clone();
    let delivered = shared.context.host.deliver_notice(&run_id, &notice);
    let (target, outcome) = match &delivered {
        Ok(run) => (run.clone(), format!("notice delivered to {run}")),
        Err(reason) => (
            run_id,
            format!("notice not delivered ({reason}); eval_report shows the result"),
        ),
    };
    shared.context.bus.emit(Event::new(DiagnosticEvent {
        source: "eval".into(),
        severity: if all_succeeded {
            DiagnosticSeverity::Info
        } else {
            DiagnosticSeverity::Warning
        },
        code: diagnostic_codes::EVALUATION_FINISHED.into(),
        detail: format!("{}; {outcome}", notice.lines().next().unwrap_or_default()),
        run_id: Some(target),
        thread_id: None,
        call_id: None,
    }));
    let mut record = lock(&record);
    record.notice = Some(outcome);
    record.save();
}

pub(super) fn completion_notice(record: &GroupRecord) -> (String, bool) {
    let lines: Vec<(bool, String)> = record.members.iter().map(member_line).collect();
    let reported = lines.iter().filter(|(ok, _)| *ok).count();
    let mut notice = format!(
        "Evaluation group {} finished: {reported} of {} evaluations produced a report.\n",
        record.id,
        lines.len()
    );
    for (_, line) in &lines {
        notice.push_str("- ");
        notice.push_str(line);
        notice.push('\n');
    }
    notice.push_str(&format!(
        "The Evaluator reads the details with eval_report {{\"group\": \"{}\"}}; summarise them for the user before deciding whether to adopt the candidate.",
        record.id
    ));
    (notice, reported == lines.len())
}

fn member_line(member: &Member) -> (bool, String) {
    let head = format!("{} {}", member.purpose.as_str(), member.thread_id);
    match &member.exit {
        Some(exit) if exit.succeeded() => {
            match read_json::<Report>(&member.dir.join(output::REPORT_JSON_FILE)) {
                Ok(report) => (
                    true,
                    format!(
                        "{head}: report in {}; {}",
                        member.dir.display(),
                        report_line(&report)
                    ),
                ),
                Err(error) => (false, format!("{head}: finished without a report: {error}")),
            }
        }
        Some(exit) => {
            let tail = tail(&member.stderr);
            let mut line = format!("{head}: failed ({})", exit.describe());
            if !tail.is_empty() {
                line.push_str(&format!("; stderr ends with: {tail}"));
            }
            (false, line)
        }
        None => (false, format!("{head}: still running")),
    }
}

pub(super) fn report_line(report: &Report) -> String {
    let tally = if report.pairs.is_empty() {
        "no pair was judged".to_owned()
    } else {
        let aspects: Vec<String> = report
            .tally
            .iter()
            .map(|tally| {
                format!(
                    "{} {}/{}/{}",
                    tally.aspect, tally.candidate_wins, tally.baseline_wins, tally.ties
                )
            })
            .collect();
        format!("candidate W/L/T by aspect: {}", aspects.join(", "))
    };
    let cost = |label| {
        report
            .cost_of(label)
            .map_or_else(|| "-".to_owned(), report::usd)
    };
    format!(
        "{tally}; cost baseline {}, candidate {}; {} warnings",
        cost(BASELINE),
        cost(CANDIDATE),
        report.warnings.len()
    )
}

fn tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or_default();
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    let skip = text.chars().count().saturating_sub(TAIL_CHARS);
    text.chars().skip(skip).collect()
}

fn member_state(member: &Member, live: bool) -> &'static str {
    match &member.exit {
        Some(exit) if exit.succeeded() => "finished",
        Some(_) => "failed",
        None if live => "running",
        None => "interrupted",
    }
}

fn record_path(root: &Path, id: &str) -> Result<PathBuf, String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("{id:?} is not an evaluation group id"));
    }
    Ok(root.join(GROUPS_DIR).join(id).join(GROUP_FILE))
}

pub(super) fn list(shared: &Shared, root: &Path, limit: usize) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(root.join(GROUPS_DIR)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids.reverse();
    ids.iter()
        .take(limit)
        .filter_map(|id| {
            let record: GroupRecord = read_json(&record_path(root, id).ok()?).ok()?;
            let live = shared.groups.is_live(id);
            Some(json!({
                "group": record.id,
                "started_at": record.started_at,
                "finished_at": record.finished_at,
                "evaluations": record.members.iter().map(|member| json!({
                    "thread_id": member.thread_id,
                    "purpose": member.purpose,
                    "dir": member.dir.file_name().map(|name| name.to_string_lossy()),
                    "state": member_state(member, live),
                })).collect::<Vec<_>>(),
            }))
        })
        .collect()
}

pub(super) fn detail(shared: &Shared, root: &Path, id: &str) -> Result<Value, String> {
    let record: GroupRecord = read_json(&record_path(root, id)?)?;
    let live = shared.groups.is_live(id);
    let mut reports = Vec::new();
    let evaluations: Vec<Value> = record
        .members
        .iter()
        .map(|member| {
            let mut value = json!({
                "thread_id": member.thread_id,
                "purpose": member.purpose,
                "repeat": member.repeat,
                "dir": member.dir.file_name().map(|name| name.to_string_lossy()),
                "state": member_state(member, live),
            });
            if let Ok(manifest) = read_json::<Manifest>(&member.dir.join(output::MANIFEST_FILE)) {
                value["progress"] = super::progress(&manifest);
            }
            match read_json::<Report>(&member.dir.join(output::REPORT_JSON_FILE)) {
                Ok(report) => {
                    value["report"] = Value::String(report_line(&report));
                    reports.push(report);
                }
                Err(_) => {
                    if let Some(exit) = member.exit.as_ref().filter(|exit| !exit.succeeded()) {
                        value["failure"] = Value::String(exit.describe());
                        value["stderr_tail"] = Value::String(tail(&member.stderr));
                    }
                }
            }
            value
        })
        .collect();
    let mut value = json!({
        "group": record.id,
        "started_at": record.started_at,
        "finished_at": record.finished_at,
        "notice": record.notice,
        "candidate": std::fs::read_to_string(&record.candidate)
            .map(|text| text.chars().take(super::ANALYSIS_CHARS).collect::<String>())
            .ok(),
        "evaluations": evaluations,
        "note": "read one evaluation in full with eval_report and its dir",
    });
    if reports.len() > 1 {
        value["table"] = Value::String(report::cross_thread_table(&reports));
    }
    Ok(value)
}
