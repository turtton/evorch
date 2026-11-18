//! Core-verified artifact references. Web may read metadata, never content.
use crate::{
    Capability,
    handshake::WireLimits,
    ids::{ProjectId, ThreadId},
    validation::{ValidationError, check_count, check_line},
};
use serde::{Deserialize, Serialize};

use super::browser::{RendererId, RendererSessionRef, RendererViewport};

pub const COMMAND_OPERATIONS: &[&str] = &["open_preview"];
pub const QUERY_OPERATIONS: &[&str] = &["list", "get", "read_content"];
pub const EVENT_TYPES: &[&str] = &[];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];
pub const MAX_CONTENT_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_ARTIFACT_PAGE: usize = 64;

/// A claim to verify against stored artifact AND conversation metadata.
/// Neither this affiliation nor knowledge of an id grants access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub project: ProjectId,
    pub thread: ThreadId,
    pub artifact_id: String,
    pub digest: String,
}

impl ArtifactRef {
    pub fn validate(&self) -> Result<(), ValidationError> {
        let valid = self
            .artifact_id
            .strip_prefix("artifact-")
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            && self.digest.len() == 64
            && self.digest.bytes().all(|b| b.is_ascii_hexdigit());
        if !valid {
            return Err(ValidationError::OutOfRange {
                field: "artifact",
                reason: "expected an artifact id and SHA-256 digest",
            });
        }
        Ok(())
    }
}

/// Deliberately excludes source paths and inline assets/body content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub reference: ArtifactRef,
    pub title: String,
    pub media_type: String,
    pub byte_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ArtifactsCommand {
    OpenPreview {
        artifact: ArtifactRef,
        session: RendererSessionRef,
        viewport: RendererViewport,
    },
}
impl ArtifactsCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::OpenPreview {
                artifact, viewport, ..
            } => {
                artifact.validate()?;
                viewport.validate()
            }
        }
    }
}

/// Generic command receipts carry no operation output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ArtifactsResult {
    Accepted { operation_id: RendererId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ArtifactsQuery {
    List {
        project: ProjectId,
        thread: ThreadId,
        after: Option<String>,
        limit: u32,
    },
    Get {
        artifact: ArtifactRef,
    },
    ReadContent {
        artifact: ArtifactRef,
        offset: u64,
        max_bytes: u32,
    },
}
impl ArtifactsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List { after, limit, .. } => {
                check_count("artifacts.limit", *limit as usize, MAX_ARTIFACT_PAGE)?;
                if let Some(after) = after {
                    check_line("artifacts.after", after, 64, false)?;
                }
                Ok(())
            }
            Self::Get { artifact } => artifact.validate(),
            Self::ReadContent {
                artifact,
                max_bytes,
                ..
            } => {
                artifact.validate()?;
                check_count(
                    "artifacts.max_bytes",
                    *max_bytes as usize,
                    MAX_CONTENT_CHUNK_BYTES.min(limits.max_body_chunk_bytes as usize),
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ArtifactsReply {
    List {
        artifacts: Vec<ArtifactMetadata>,
        next: Option<String>,
    },
    Metadata(ArtifactMetadata),
    Content {
        artifact: ArtifactRef,
        offset: u64,
        /// Original UTF-8 HTML/text, never image artifact bytes.
        text: String,
        eof: bool,
    },
}
impl ArtifactsReply {
    pub fn validate(&self, limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::List { artifacts, next } => {
                check_count("artifacts", artifacts.len(), MAX_ARTIFACT_PAGE)?;
                for item in artifacts {
                    metadata(item)?;
                }
                if let Some(next) = next {
                    check_line("artifacts.next", next, 64, false)?;
                }
            }
            Self::Metadata(item) => metadata(item)?,
            Self::Content { artifact, text, .. } => {
                artifact.validate()?;
                check_count(
                    "artifacts.content",
                    text.len(),
                    MAX_CONTENT_CHUNK_BYTES.min(limits.max_body_chunk_bytes as usize),
                )?;
            }
        }
        super::browser::validate_wire_size(self, limits)
    }
}
fn metadata(item: &ArtifactMetadata) -> Result<(), ValidationError> {
    item.reference.validate()?;
    check_line("artifact.title", &item.title, 1024, false)?;
    check_line("artifact.media_type", &item.media_type, 128, false)
}

no_events!(ArtifactsEvent);
