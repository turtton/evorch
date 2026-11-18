use super::AreaSamples;
use workbench_protocol::{
    commands::Command,
    namespaces::{artifacts::*, browser::*},
    queries::{Query, QueryReply},
};

pub fn reference() -> ArtifactRef {
    ArtifactRef {
        project: workbench_protocol::ids::ProjectId::new("project-1").unwrap(),
        thread: workbench_protocol::ids::ThreadId::new("thread-1").unwrap(),
        artifact_id: "artifact-1234567890abcdef1234567890abcdef".into(),
        digest: "a".repeat(64),
    }
}
pub fn id() -> RendererId {
    RendererId::new("1234567890abcdef1234567890abcdef").unwrap()
}
pub fn session() -> RendererSessionRef {
    RendererSessionRef {
        core_epoch: 17,
        id: id(),
    }
}
pub fn viewport() -> RendererViewport {
    RendererViewport {
        width: 1024,
        height: 768,
    }
}
pub fn metadata() -> ArtifactMetadata {
    ArtifactMetadata {
        reference: reference(),
        title: "Offline chart".into(),
        media_type: "text/html".into(),
        byte_len: 12,
    }
}
pub fn samples() -> AreaSamples {
    let mut out = AreaSamples::none("artifacts");
    out.commands = vec![Command::Artifacts(ArtifactsCommand::OpenPreview {
        artifact: reference(),
        session: session(),
        viewport: viewport(),
    })];
    out.queries = vec![
        ArtifactsQuery::List {
            project: reference().project,
            thread: reference().thread,
            after: None,
            limit: 16,
        },
        ArtifactsQuery::Get {
            artifact: reference(),
        },
        ArtifactsQuery::ReadContent {
            artifact: reference(),
            offset: 0,
            max_bytes: 1024,
        },
    ]
    .into_iter()
    .map(Query::Artifacts)
    .collect();
    out.query_replies = vec![
        ArtifactsReply::List {
            artifacts: vec![metadata()],
            next: None,
        },
        ArtifactsReply::Metadata(metadata()),
        ArtifactsReply::Content {
            artifact: reference(),
            offset: 0,
            text: "<html>".into(),
            eof: true,
        },
    ]
    .into_iter()
    .map(QueryReply::Artifacts)
    .collect();
    out
}
