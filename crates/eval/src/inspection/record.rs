//! The group record an Evaluator launch writes (`group.json`) and the monitor
//! of the groups this process launched.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::output;

pub(crate) const GROUPS_DIR: &str = ".groups";
pub(crate) const GROUP_FILE: &str = "group.json";
pub(crate) const OVERLAY_FILE: &str = "candidate.toml";
/// The version of `group.json` this build writes and reads.
pub(crate) const GROUP_SCHEMA: u64 = 1;

const fn current_schema() -> u64 {
    GROUP_SCHEMA
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Target,
    Regression,
}

impl Purpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Regression => "regression",
        }
    }
}

/// How an evaluation process ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Why the process could not be started or waited for.
    pub error: Option<String>,
    pub finished_at: String,
}

impl ProcessExit {
    pub(crate) fn now(code: Option<i32>, signal: Option<i32>, error: Option<String>) -> Self {
        Self {
            code,
            signal,
            error,
            finished_at: output::timestamp(SystemTime::now()),
        }
    }

    pub(crate) fn of(status: std::process::ExitStatus) -> Self {
        use std::os::unix::process::ExitStatusExt;
        Self::now(status.code(), status.signal(), None)
    }

    pub fn succeeded(&self) -> bool {
        self.error.is_none() && self.code == Some(0)
    }

    pub fn describe(&self) -> String {
        match (&self.error, self.code, self.signal) {
            (Some(error), ..) => error.clone(),
            (None, Some(code), _) => format!("exit code {code}"),
            (None, None, Some(signal)) => format!("killed by signal {signal}"),
            (None, None, None) => "exited".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Member {
    pub thread_id: String,
    pub purpose: Purpose,
    pub repeat: u32,
    pub dir: PathBuf,
    pub pid: Option<u32>,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub exit: Option<ProcessExit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupRecord {
    #[serde(default = "current_schema")]
    pub schema: u64,
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

    pub(crate) fn save(&self) {
        let Some(dir) = self.dir() else { return };
        if let Err(error) = output::write_json(&dir.join(GROUP_FILE), self) {
            tracing::warn!(%error, group = %self.id, "evaluation group record not saved");
        }
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What this process knows of one evaluation process it launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// Launched by this process and not yet waited for.
    Running,
    Exited(ProcessExit),
}

/// A read-only view of the groups this process launched. Only a process that
/// started an evaluation can say it is still running: the record on disk keeps
/// no exit for a process that is running and for one nobody saw end alike.
#[derive(Clone, Default)]
pub struct LaunchMonitor {
    live: Arc<Mutex<HashMap<String, Arc<Mutex<GroupRecord>>>>>,
}

impl LaunchMonitor {
    pub(crate) fn insert(&self, record: Arc<Mutex<GroupRecord>>) {
        let id = lock(&record).id.clone();
        lock(&self.live).insert(id, record);
    }

    /// Whether this process launched the group.
    pub fn launched(&self, group_id: &str) -> bool {
        lock(&self.live).contains_key(group_id)
    }

    /// The state of each member's process, in the group's member order, or
    /// `None` for a group this process did not launch.
    pub fn processes(&self, group_id: &str) -> Option<Vec<ProcessState>> {
        let record = lock(&self.live).get(group_id).cloned()?;
        let record = lock(&record);
        Some(
            record
                .members
                .iter()
                .map(|member| {
                    member
                        .exit
                        .clone()
                        .map_or(ProcessState::Running, ProcessState::Exited)
                })
                .collect(),
        )
    }

    /// Output directories of the members still running.
    pub(crate) fn running_dirs(&self) -> HashSet<OsString> {
        lock(&self.live)
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
