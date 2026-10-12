//! Asynchronous shell jobs seen by one transcript: what each job runs, its
//! state, and its output log.
//!
//! Live output events carry the redacted stream with offsets and are not
//! persisted. Restored history only has the start and poll results the agent
//! received, so those cursor-ordered deltas form the fallback log.

use std::collections::VecDeque;

/// Per-job log bound; older output is dropped from the front.
const LOG_BYTES: usize = 256 * 1024;
const MAX_JOBS: usize = 64;
const GAP_MARKER: &str = "[… earlier output expired …]\n";

/// Keep counter handles distinct; only historical UUIDs use a short prefix.
pub fn short_job_id(id: &str) -> &str {
    if id.starts_with("job-") {
        id
    } else {
        id.get(..8).unwrap_or(id)
    }
}

/// GUI identity, never a model-facing shell control handle.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShellJobKey {
    pub run_id: Option<String>,
    pub handle: String,
    /// Separates historical jobs from a restored run that reused an old handle.
    pub uid: Option<String>,
}

impl ShellJobKey {
    pub fn new(run_id: Option<&str>, handle: &str, uid: Option<&str>) -> Self {
        Self {
            run_id: run_id.map(str::to_owned),
            handle: handle.to_owned(),
            uid: uid.map(str::to_owned),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellJob {
    pub key: ShellJobKey,
    pub id: String,
    pub command: Option<String>,
    pub run_id: Option<String>,
    /// `running`, `completed`, `failed`, `timed_out` or `cancelled`.
    pub status: String,
    pub exit_code: Option<i32>,
    /// Saved full output once the job finished, when the tool kept one.
    pub artifact: Option<String>,
    live: Option<LiveLog>,
    polled: String,
    polled_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveLog {
    text: String,
    /// Stream offset just past `text`.
    end: u64,
    truncated: bool,
}

impl ShellJob {
    fn new(key: ShellJobKey) -> Self {
        Self {
            id: key.handle.clone(),
            run_id: key.run_id.clone(),
            key,
            command: None,
            status: "running".into(),
            exit_code: None,
            artifact: None,
            live: None,
            polled: String::new(),
            polled_truncated: false,
        }
    }

    pub fn is_running(&self) -> bool {
        self.status == "running"
    }

    pub fn short_id(&self) -> &str {
        short_job_id(&self.id)
    }

    /// Live output when it streamed, otherwise what polls returned.
    pub fn log(&self) -> &str {
        self.live
            .as_ref()
            .map_or(self.polled.as_str(), |live| live.text.as_str())
    }

    /// Whether the beginning of the output is no longer in [`Self::log`].
    pub fn log_truncated(&self) -> bool {
        self.live
            .as_ref()
            .map_or(self.polled_truncated, |live| live.truncated)
    }

    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// The last `count` non-empty lines of the log.
    pub fn tail(&self, count: usize) -> Vec<&str> {
        let mut lines: Vec<_> = self
            .log()
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(count)
            .collect();
        lines.reverse();
        lines
    }

    /// Short outcome label such as `running`, `exit 0` or `timed out`.
    pub fn outcome(&self) -> String {
        match (self.status.as_str(), self.exit_code) {
            (_, Some(code)) => format!("exit {code}"),
            (status, None) => status.replace('_', " "),
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.status.as_str(), "failed" | "timed_out" | "cancelled")
    }

    fn set_status(&mut self, status: &str, exit_code: Option<i32>) {
        // Poll results can be delivered after the live stream already ended.
        if self.is_running() || status != "running" {
            self.status = status.to_owned();
            if exit_code.is_some() {
                self.exit_code = exit_code;
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShellJobs {
    jobs: VecDeque<ShellJob>,
}

impl ShellJobs {
    /// Legacy lookup is deliberately unavailable when the handle is ambiguous.
    pub fn get(&self, id: &str) -> Option<&ShellJob> {
        let mut matches = self.jobs.iter().filter(|job| job.id == id);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn get_key(&self, key: &ShellJobKey) -> Option<&ShellJob> {
        self.jobs.iter().find(|job| &job.key == key)
    }

    /// Resolves a control call before its result supplies the job UUID.
    /// Historical instances make the handle ambiguous even within one run.
    pub fn get_scoped(&self, run_id: Option<&str>, handle: &str) -> Option<&ShellJob> {
        let mut matches = self
            .jobs
            .iter()
            .filter(|job| job.key.run_id.as_deref() == run_id && job.key.handle == handle);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    /// Jobs in the order they were first seen.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &ShellJob> {
        self.jobs.iter()
    }

    fn job_mut(&mut self, key: ShellJobKey) -> &mut ShellJob {
        if let Some(index) = self.jobs.iter().position(|job| job.key == key) {
            return &mut self.jobs[index];
        }
        if self.jobs.len() >= MAX_JOBS {
            let evict = self
                .jobs
                .iter()
                .position(|job| !job.is_running())
                .unwrap_or(0);
            self.jobs.remove(evict);
        }
        self.jobs.push_back(ShellJob::new(key));
        self.jobs.back_mut().expect("job was just pushed")
    }

    pub(crate) fn apply_live(
        &mut self,
        key: ShellJobKey,
        offset: u64,
        chunk: &str,
        status: &str,
        exit_code: Option<i32>,
    ) {
        let job = self.job_mut(key);
        job.set_status(status, exit_code);
        let live = job.live.get_or_insert_with(|| LiveLog {
            text: String::new(),
            end: offset,
            truncated: offset > 0,
        });
        let chunk = if offset < live.end {
            // Already shown; keep only the unseen suffix.
            let mut skip = usize::try_from(live.end - offset).unwrap_or(usize::MAX);
            if skip >= chunk.len() {
                return;
            }
            while !chunk.is_char_boundary(skip) {
                skip += 1;
            }
            &chunk[skip..]
        } else {
            if offset > live.end {
                live.text.push_str(GAP_MARKER);
            }
            chunk
        };
        live.text.push_str(chunk);
        live.end = offset.max(live.end) + chunk.len() as u64;
        live.truncated |= bound(&mut live.text);
    }

    /// Records a shell start, poll, stdin or stop result that names a job.
    pub(crate) fn apply_result(
        &mut self,
        input: Option<&serde_json::Value>,
        output: Option<&str>,
        detail: Option<&serde_json::Value>,
    ) {
        let Some(state) = detail.and_then(|detail| detail.get("shell_job")) else {
            return;
        };
        let Some(job_id) = state.get("job_id").and_then(serde_json::Value::as_str) else {
            return;
        };
        let key = ShellJobKey::new(
            state.get("run_id").and_then(serde_json::Value::as_str),
            job_id,
            state.get("job_uid").and_then(serde_json::Value::as_str),
        );
        let job = self.job_mut(key);
        if let Some(command) = input
            .and_then(|input| input.get("command"))
            .and_then(serde_json::Value::as_str)
        {
            job.command = Some(command.to_owned());
        }
        if let Some(status) = state.get("status").and_then(serde_json::Value::as_str) {
            let exit_code = state
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            job.set_status(status, exit_code);
        }
        if let Some(path) = detail
            .and_then(|detail| detail.pointer("/output_artifact/path"))
            .and_then(serde_json::Value::as_str)
        {
            job.artifact = Some(path.to_owned());
        }
        if state.get("output_gap").and_then(serde_json::Value::as_bool) == Some(true) {
            job.polled.push_str(GAP_MARKER);
        }
        if let Some(body) = output.map(result_body) {
            job.polled.push_str(body);
            job.polled_truncated |= bound(&mut job.polled);
        }
    }
}

/// Output text of a job result without its `key: value` status preamble.
pub fn result_body(output: &str) -> &str {
    let mut rest = output;
    loop {
        let Some((line, next)) = rest.split_once('\n') else {
            return if is_preamble(rest) { "" } else { rest };
        };
        if !is_preamble(line) {
            return rest;
        }
        rest = next;
    }
}

fn is_preamble(line: &str) -> bool {
    ["shell job: ", "status: ", "cursor: ", "exit_code: "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        || line.starts_with("[Earlier live output expired")
}

/// Drops whole lines from the front until `text` fits; true when it dropped.
fn bound(text: &mut String) -> bool {
    if text.len() <= LOG_BYTES {
        return false;
    }
    let mut cut = text.len() - LOG_BYTES;
    cut = text[cut..].find('\n').map_or(cut, |index| cut + index + 1);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    text.drain(..cut);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn started(jobs: &mut ShellJobs) {
        jobs.apply_result(
            Some(&json!({"command": "cargo test"})),
            Some("shell job: job-12345678\nstatus: running\ncursor: 6\nfirst\n"),
            Some(&json!({"shell_job": {"job_id": "job-12345678", "run_id": "run-1", "status": "running"}})),
        );
    }

    #[test]
    fn poll_results_build_a_fallback_log_without_status_lines() {
        let mut jobs = ShellJobs::default();
        started(&mut jobs);
        jobs.apply_result(
            Some(&json!({"action": "poll", "job_id": "job-12345678"})),
            Some("shell job: job-12345678\nstatus: failed\ncursor: 12\nexit_code: 2\nsecond\n"),
            Some(&json!({
                "shell_job": {"job_id": "job-12345678", "run_id": "run-1", "status": "failed", "exit_code": 2},
                "output_artifact": {"path": "/tmp/out.log"}
            })),
        );
        let job = jobs.get("job-12345678").unwrap();
        assert_eq!(job.command.as_deref(), Some("cargo test"));
        assert_eq!(job.log(), "first\nsecond\n");
        assert_eq!(job.outcome(), "exit 2");
        assert!(job.failed());
        assert_eq!(job.artifact.as_deref(), Some("/tmp/out.log"));
        assert_eq!(job.short_id(), "job-12345678");
    }

    #[test]
    fn live_output_takes_precedence_and_skips_overlap_and_marks_gaps() {
        let mut jobs = ShellJobs::default();
        started(&mut jobs);
        jobs.apply_live(
            ShellJobKey::new(Some("run-1"), "job-12345678", None),
            0,
            "one\n",
            "running",
            None,
        );
        jobs.apply_live(
            ShellJobKey::new(Some("run-1"), "job-12345678", None),
            2,
            "e\ntwo\n",
            "running",
            None,
        );
        jobs.apply_live(
            ShellJobKey::new(Some("run-1"), "job-12345678", None),
            20,
            "late\n",
            "completed",
            Some(0),
        );
        let job = jobs.get("job-12345678").unwrap();
        assert_eq!(job.log(), format!("one\ntwo\n{GAP_MARKER}late\n"));
        assert_eq!(job.tail(2), vec![GAP_MARKER.trim_end(), "late"]);
        assert_eq!(job.run_id.as_deref(), Some("run-1"));
        assert!(!job.is_running());
    }

    #[test]
    fn stale_running_result_does_not_reopen_a_finished_job() {
        let mut jobs = ShellJobs::default();
        jobs.apply_live(
            ShellJobKey::new(Some("run-1"), "job-12345678", None),
            0,
            "",
            "completed",
            Some(0),
        );
        started(&mut jobs);
        let job = jobs.get("job-12345678").unwrap();
        assert_eq!((job.status.as_str(), job.exit_code), ("completed", Some(0)));
    }

    #[test]
    fn log_is_bounded_by_whole_lines() {
        let mut jobs = ShellJobs::default();
        let line = "x".repeat(1023) + "\n";
        for index in 0..300u64 {
            jobs.apply_live(
                ShellJobKey::new(None, "job", None),
                index * 1024,
                &line,
                "running",
                None,
            );
        }
        let job = jobs.get("job").unwrap();
        assert!(job.log().len() <= LOG_BYTES);
        assert!(job.log().starts_with('x'));
        assert!(job.log_truncated());
    }
}
