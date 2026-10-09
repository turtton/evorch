//! Artifacts a conversation owner presented to the user. Presentation grants no authority.
use serde::{Deserialize, Serialize};

/// One stored artifact as it was captured by `render_artifact`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentedArtifact {
    pub artifact_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// `image/png`, `image/jpeg`, `image/gif`, `image/webp` or `text/html`.
    pub media_type: String,
    /// Immutable content-addressed copy, never the agent's working file.
    pub path: String,
    pub byte_len: u64,
    pub sha256: String,
}

impl PresentedArtifact {
    pub fn is_image(&self) -> bool {
        self.media_type.starts_with("image/")
    }
}

/// A group of artifacts shown together in the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPresentation {
    pub presentation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub artifacts: Vec<PresentedArtifact>,
}

#[cfg(test)]
mod tests {
    use crate::{Event, EventKind, ToolEvent};

    #[test]
    fn presentation_events_round_trip_through_storage_json() {
        let event = Event::new(ToolEvent::ArtifactsPresented {
            run_id: "7".into(),
            presentation: super::ArtifactPresentation {
                presentation_id: "presentation-1".into(),
                title: None,
                caption: Some("Compare".into()),
                artifacts: vec![super::PresentedArtifact {
                    artifact_id: "artifact-1".into(),
                    title: "A".into(),
                    caption: None,
                    media_type: "image/png".into(),
                    path: "/store/blobs/a.png".into(),
                    byte_len: 3,
                    sha256: "a".into(),
                }],
            },
        });
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"]["payload"]["kind"], "ArtifactsPresented");
        let decoded: Event = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, event);
        let EventKind::Tool(ToolEvent::ArtifactsPresented { presentation, .. }) = decoded.kind
        else {
            panic!("unexpected event");
        };
        assert!(presentation.artifacts[0].is_image());
    }
}
