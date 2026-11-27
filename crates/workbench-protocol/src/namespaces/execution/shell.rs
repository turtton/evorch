//! Read-only, memory-only shell observation; never a shell control/ACK API.
use crate::{
    ids::{RunId, ThreadId},
    limits::MAX_NAME_BYTES,
    validation::{ValidationError, check_bytes},
};
use serde::{Deserialize, Serialize};

pub const MAX_SHELL_COMMAND_BYTES: usize = 4096;
pub const MAX_SHELL_TAIL_BYTES: usize = 64 * 1024;
pub const MAX_SHELL_OUTPUT_EVENT_BYTES: usize = 32 * 1024;
/// Old terminal jobs are evicted oldest-first. Active jobs are not evicted.
pub const MAX_RETAINED_SHELL_JOBS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellJobState {
    Running,
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellJobView {
    pub thread: ThreadId,
    pub run: RunId,
    pub job_uid: String,
    pub handle: String,
    pub source_call_id: Option<String>,
    pub command: Option<String>,
    pub state: ShellJobState,
    pub exit_code: Option<i32>,
}
impl ShellJobView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        identity(&self.job_uid)?;
        check_bytes("handle", &self.handle, MAX_NAME_BYTES, false)?;
        if let Some(call) = &self.source_call_id {
            check_bytes("source_call_id", call, MAX_NAME_BYTES, false)?;
        }
        if let Some(command) = &self.command {
            check_bytes("command", command, MAX_SHELL_COMMAND_BYTES, true)?;
        }
        Ok(())
    }
}
pub(super) fn identity(uid: &str) -> Result<(), ValidationError> {
    crate::ids::validate_id("job_uid", uid)
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadShellOutput {
    pub thread: ThreadId,
    pub run: RunId,
    pub job_uid: String,
    /// Absolute UTF-8 byte offset; a gap is reported by the returned offset.
    #[serde(with = "crate::wire")]
    pub offset: u64,
    pub max_bytes: u32,
}
impl ReadShellOutput {
    pub fn validate(&self) -> Result<(), ValidationError> {
        identity(&self.job_uid)?;
        if self.max_bytes == 0 || self.max_bytes as usize > MAX_SHELL_TAIL_BYTES {
            return Err(ValidationError::OutOfRange {
                field: "max_bytes",
                reason: "expected 1..=64 KiB",
            });
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellOutput {
    pub thread: ThreadId,
    pub run: RunId,
    pub job_uid: String,
    #[serde(with = "crate::wire")]
    pub available_from: u64,
    #[serde(with = "crate::wire")]
    pub offset: u64,
    pub chunk: String,
    #[serde(with = "crate::wire")]
    pub output_end: u64,
}
impl ShellOutput {
    pub fn validate(&self) -> Result<(), ValidationError> {
        identity(&self.job_uid)?;
        check_bytes("chunk", &self.chunk, MAX_SHELL_TAIL_BYTES, true)?;
        if self.available_from > self.offset
            || self
                .offset
                .checked_add(self.chunk.len() as u64)
                .is_none_or(|end| end > self.output_end)
        {
            return Err(ValidationError::Inconsistent {
                field: "offset",
                reason: "invalid retained output range",
            });
        }
        Ok(())
    }
}
