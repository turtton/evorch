//! Evaluations the Evaluator starts: one background `evorch eval run` per
//! thread, launched together as a group and watched until every process has
//! exited, then reported to the run that started them (ADR 0036 §1).
//!
//! A group lives in `<eval dir>/.groups/<id>/`: `group.json`, the candidate
//! overlay and each process's output. The processes end with the GUI process,
//! so a group a later GUI session finds unfinished was interrupted.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use event_bus::event::diagnostic_codes;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use serde_json::{Value, json};

use super::{Shared, read_json};
use crate::output;
use crate::report::{self, Report};
use crate::runner::{BASELINE, CANDIDATE};

const CLI: &str = "evorch";
const TAIL_BYTES: u64 = 16 * 1024;
const TAIL_CHARS: usize = 1_200;

pub(super) use crate::inspection::Purpose;
use crate::inspection::{
    Artifact, EvaluationGroupSnapshot, EvaluationInspector, EvaluationMemberSnapshot, GROUP_SCHEMA,
    GROUPS_DIR, GroupRecord, Member, OVERLAY_FILE, ProcessExit as Exit, lock,
};

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
    // A background evaluation must not be stopped by the terminal the GUI was
    // started from, nor hold the GUI's descriptors.
    sandbox::process::apply_launch_policy(command.as_std_mut());
    end_with_parent(&mut command);
    command.kill_on_drop(true);
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
    launch_with(shared, run_id, overlay, launches, spawn)
}

fn launch_with(
    shared: &Arc<Shared>,
    run_id: &str,
    overlay: &str,
    launches: Vec<Launch>,
    mut spawn_child: impl FnMut(&Path, &[OsString], &Member) -> std::io::Result<tokio::process::Child>,
) -> Result<GroupRecord, String> {
    let mut admission = shared.lifecycle.state.lock().unwrap();
    if admission.closed {
        return Err("evaluation host is stopping".into());
    }
    shared.context.host.authorize_run(run_id)?;
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
        match spawn_child(&cli, &args, &member) {
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
        schema: GROUP_SCHEMA,
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
        admission.tasks.push(tokio::spawn(watch(
            shared.context.clone(),
            shared.lifecycle.stop.subscribe(),
            Arc::clone(&record),
            index,
            child,
        )));
    }
    Ok(snapshot)
}

async fn watch(
    context: super::EvaluatorContext,
    mut stopped: tokio::sync::watch::Receiver<bool>,
    record: Arc<Mutex<GroupRecord>>,
    index: usize,
    mut child: tokio::process::Child,
) {
    let stop = async {
        loop {
            if *stopped.borrow_and_update() {
                break;
            }
            if stopped.changed().await.is_err() {
                break;
            }
        }
    };
    let status = tokio::select! {
        biased;
        () = stop => {
            let _ = child.start_kill();
            child.wait().await
        }
        status = child.wait() => status,
    };
    let exit = match status {
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
    let delivered = if *stopped.borrow() {
        Err("evaluation host stopped; notice retained in report".into())
    } else {
        context.host.deliver_notice(&run_id, &notice)
    };
    let (target, outcome) = match &delivered {
        Ok(run) => (run.clone(), format!("notice delivered to {run}")),
        Err(reason) => (
            run_id,
            format!("notice not delivered ({reason}); eval_report shows the result"),
        ),
    };
    context.bus.emit(Event::new(DiagnosticEvent {
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

fn inspector(shared: &Shared, root: &Path) -> EvaluationInspector {
    EvaluationInspector::new(root).with_monitor(shared.groups.clone())
}

pub(super) fn list(shared: &Shared, root: &Path, limit: usize) -> Vec<Value> {
    inspector(shared, root)
        .list_groups(limit)
        .entries
        .into_iter()
        .filter_map(|entry| entry.group.into_present())
        .map(|group| {
            json!({
                "group": group.id,
                "started_at": group.started_at,
                "finished_at": group.finished_at,
                "evaluations": group.members.iter().map(|member| json!({
                    "thread_id": member.thread_id,
                    "purpose": member.purpose,
                    "dir": member.dir_name,
                    "state": member.phase.legacy_state(),
                    "phase": member.phase.as_str(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn evaluation_value(member: &EvaluationMemberSnapshot, report: Option<&Report>) -> Value {
    let mut value = json!({
        "thread_id": member.thread_id,
        "purpose": member.purpose,
        "repeat": member.repeat,
        "dir": member.dir_name,
        "state": member.phase.legacy_state(),
        "phase": member.phase.as_str(),
    });
    if let Some(progress) = member.progress {
        value["progress"] = super::progress_value(
            progress.planned,
            progress.started,
            progress.finished,
            progress.manifest_finished,
        );
    }
    if let Some(report) = report {
        value["report"] = Value::String(report_line(report));
    } else if let Some(failure) = &member.failure {
        value["failure"] = Value::String(failure.clone());
        value["stderr_tail"] = Value::String(member.stderr_tail.clone().unwrap_or_default());
    }
    value
}

fn group_value(group: &EvaluationGroupSnapshot, reports: &[Option<Report>]) -> Value {
    let evaluations: Vec<Value> = group
        .members
        .iter()
        .zip(reports)
        .map(|(member, report)| evaluation_value(member, report.as_ref()))
        .collect();
    let mut value = json!({
        "group": group.id,
        "started_at": group.started_at,
        "finished_at": group.finished_at,
        "notice": group.notice,
        "candidate": group
            .candidate
            .present()
            .map(|overlay| overlay.text.chars().take(super::ANALYSIS_CHARS).collect::<String>()),
        "evaluations": evaluations,
        "note": "read one evaluation in full with eval_report and its dir",
    });
    let reports: Vec<Report> = reports.iter().flatten().cloned().collect();
    if reports.len() > 1 {
        value["table"] = Value::String(report::cross_thread_table(&reports));
    }
    value
}

pub(super) fn detail(shared: &Shared, root: &Path, id: &str) -> Result<Value, String> {
    match inspector(shared, root).inspect_group(id, false) {
        Artifact::Present(inspected) => Ok(group_value(&inspected.snapshot, &inspected.reports)),
        Artifact::Missing => Err(format!("no evaluation group {id:?}")),
        Artifact::Problem(problem) => Err(problem.to_string()),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod launch_tests {
    use std::os::fd::AsRawFd;

    use super::*;

    #[tokio::test]
    async fn an_evaluation_starts_in_its_own_session_without_the_launchers_descriptors() {
        // Given: the launcher (the GUI) holds a descriptor without O_CLOEXEC.
        let dir = tempfile::tempdir().unwrap();
        let held = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        let fd = rustix::io::fcntl_dupfd_cloexec(&held, 100).unwrap();
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::empty()).unwrap();
        let member = Member {
            thread_id: "thread-launch".into(),
            purpose: Purpose::Target,
            repeat: 1,
            dir: dir.path().join("evaluation"),
            pid: None,
            stdout: dir.path().join("stdout.log"),
            stderr: dir.path().join("stderr.log"),
            exit: None,
        };
        let sh = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("sh"))
            .find(|sh| sh.is_file())
            .expect("sh on PATH");
        let probe = format!(
            "echo $$; cut -d' ' -f6 /proc/$$/stat; readlink /proc/$$/fd/{} || echo descriptor-closed",
            fd.as_raw_fd()
        );

        // When: the evaluation process reports its pid, session and that descriptor.
        let mut child = spawn(&sh, &["-c".into(), probe.into()], &member).unwrap();
        assert!(child.wait().await.unwrap().success());

        // Then: it leads a session of its own and the descriptor did not reach it.
        let stdout = std::fs::read_to_string(&member.stdout).unwrap();
        let lines: Vec<&str> = stdout.lines().collect();
        let [pid, session, descriptor] = lines[..] else {
            panic!("{stdout}");
        };
        assert!(pid.parse::<u32>().is_ok(), "{stdout}");
        assert_eq!(session, pid, "{stdout}");
        assert_eq!(descriptor, "descriptor-closed", "{stdout}");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Host(AtomicUsize);
    impl crate::evaluator::EvaluatorHost for Host {
        fn deliver_notice(&self, _: &str, _: &str) -> Result<String, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("notice".into())
        }
        fn reload_config(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn partial_child_start_is_reaped_and_saved_before_service_drain_completes() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("test.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let host = Arc::new(Host(AtomicUsize::new(0)));
        let lifecycle = crate::evaluator::EvaluationService::default();
        let sh = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join("sh"))
            .find(|sh| sh.is_file())
            .unwrap();
        let shared = Arc::new(Shared {
            context: crate::evaluator::EvaluatorContext {
                database: config.db_path,
                storage: storage.handle(),
                user_config_dir: dir.path().into(),
                catalog_dir: None,
                bus: Arc::new(event_bus::EventBus::new(32)),
                host: host.clone(),
                cli: Some(sh),
            },
            catalog: tokio::sync::OnceCell::new(),
            groups: Default::default(),
            lifecycle: lifecycle.clone(),
        });
        let launches = || {
            ["started", "refused"]
                .into_iter()
                .map(|thread_id| Launch {
                    thread_id: thread_id.into(),
                    purpose: Purpose::Target,
                    repeat: 1,
                    problem: "test".into(),
                })
                .collect()
        };
        let mut count = 0;
        let group = launch_with(&shared, "run-1", "overlay", launches(), |cli, _, member| {
            count += 1;
            if count == 2 {
                return Err(std::io::Error::other("injected spawn failure"));
            }
            // The shell blocks itself, not by sleeping; shutdown can kill it even
            // before exec. Child wait is the completion signal, not elapsed time.
            spawn(cli, &["-c".into(), "kill -STOP $$".into()], member)
        })
        .unwrap();
        assert!(
            lifecycle.is_busy(),
            "a real evaluation child and its report task are unfinished"
        );
        assert!(group.members[0].exit.is_none());
        assert!(group.members[1].exit.is_some());
        lifecycle.shutdown().await.unwrap();
        lifecycle.wait_drained().await;
        assert!(!lifecycle.is_busy());
        let group = detail(&shared, &dir.path().join("eval"), &group.id).unwrap();
        assert!(!group["finished_at"].is_null());
        assert_eq!(
            host.0.load(Ordering::SeqCst),
            0,
            "no shutdown notice may start a new run"
        );
        assert!(
            launch(&shared, "run-1", "overlay", launches())
                .unwrap_err()
                .contains("stopping")
        );
        lifecycle.shutdown().await.unwrap();
    }
}
