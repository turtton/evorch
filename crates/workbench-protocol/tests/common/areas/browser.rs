use super::{
    AreaSamples,
    artifacts::{id, reference, session, viewport},
};
use serde_json::json;
use workbench_protocol::{
    commands::Command,
    events::UiEvent,
    namespaces::browser::*,
    queries::{Query, QueryReply},
};
fn tab_ref() -> RendererTabRef {
    RendererTabRef {
        core_epoch: 17,
        generation: 2,
        id: id(),
    }
}
fn tab() -> RendererTab {
    RendererTab {
        reference: tab_ref(),
        session: session(),
        owner: RendererOwnerKind::ClientPreview,
        artifact: reference(),
        viewport: viewport(),
        fps: 30,
        streaming: false,
    }
}
pub fn samples() -> AreaSamples {
    let mut out = AreaSamples::none("browser");
    out.commands = vec![
        BrowserCommand::SubscribeAgentFrames {
            scope: observation_scope(),
        },
        BrowserCommand::UnsubscribeAgentFrames {
            subscription: observation_id(),
        },
        BrowserCommand::BindAgentData {
            subscription: observation_id(),
            token: observation_id(),
            channel_id: id(),
        },
        BrowserCommand::CreateRendererSession {},
        BrowserCommand::ResumeRendererSession {
            session: session(),
            token: id(),
        },
        BrowserCommand::CloseRendererSession { session: session() },
        BrowserCommand::ResizeRendererTab {
            tab: tab_ref(),
            viewport: viewport(),
        },
        BrowserCommand::SetRendererStreaming {
            tab: tab_ref(),
            enabled: false,
        },
        BrowserCommand::SetRendererFrameRate {
            tab: tab_ref(),
            fps: 10,
        },
        BrowserCommand::RendererCdp {
            tab: tab_ref(),
            method: "Runtime.evaluate".into(),
            params: json!({"expression":"1+1"}),
        },
        BrowserCommand::SettleRendererTab { tab: tab_ref() },
        BrowserCommand::TakeRendererNotices { tab: tab_ref() },
        BrowserCommand::CaptureRendererTab { tab: tab_ref() },
        BrowserCommand::CloseRendererTab { tab: tab_ref() },
        BrowserCommand::GrantRendererObserver {
            tab: tab_ref(),
            client: workbench_protocol::ids::ClientId::new("client-b").unwrap(),
            metadata: true,
            frames: false,
        },
        BrowserCommand::SubscribeRendererFrames { tab: tab_ref() },
        BrowserCommand::UnsubscribeRendererFrames { subscription: id() },
        BrowserCommand::BindRendererData {
            channel_id: id(),
            source: RendererDataSource::Frames {
                subscription: id(),
                token: id(),
            },
        },
    ]
    .into_iter()
    .map(Command::Browser)
    .collect();
    out.queries = vec![
        BrowserQuery::ListAgentRuns {
            project: observation_scope().project,
            thread: observation_scope().thread,
        },
        BrowserQuery::GetAgentObservation {
            scope: observation_scope(),
        },
        BrowserQuery::GetAgentSubscription {
            subscription: observation_id(),
        },
        BrowserQuery::GetRendererStatus {},
        BrowserQuery::ListRendererTabs {
            offset: 0,
            limit: 4,
        },
        BrowserQuery::GetRendererTab { tab: tab_ref() },
        BrowserQuery::GetRendererOperation { operation_id: id() },
    ]
    .into_iter()
    .map(Query::Browser)
    .collect();
    out.ui_events = vec![
        BrowserEvent::AgentObservationChanged(observation::ObservationEvent {
            subscription: observation_id(),
            revision: 1,
            state: observation::ObservationState::Live,
        }),
        BrowserEvent::RendererChanged { id: id() },
        BrowserEvent::TabChanged { id: id() },
        BrowserEvent::OperationChanged { id: id() },
    ]
    .into_iter()
    .map(UiEvent::Browser)
    .collect();
    out.query_replies = vec![
        BrowserReply::AgentObservation(observation::ObservationReply::Runs {
            scopes: vec![observation_scope()],
        }),
        BrowserReply::Status(RendererStatus {
            core_epoch: 17,
            generation: 2,
            phase: RendererPhase::Ready,
            tabs_used: 1,
            tabs_limit: 4,
        }),
        BrowserReply::Tabs {
            tabs: vec![tab()],
            more: false,
        },
        BrowserReply::Tab(Box::new(tab())),
        BrowserReply::Operation(RendererOperation {
            operation_id: id(),
            state: RendererOperationState::Completed {
                output: RendererOutput::Cdp {
                    result: json!({"result":{"type":"number","value":2}}),
                },
            },
        }),
    ]
    .into_iter()
    .map(QueryReply::Browser)
    .collect();
    out
}

fn observation_id() -> observation::ObservationId {
    observation::ObservationId::new("a".repeat(32)).unwrap()
}
fn observation_scope() -> observation::ObservationScope {
    observation::ObservationScope {
        project: workbench_protocol::ids::ProjectId::new("project").unwrap(),
        thread: workbench_protocol::ids::ThreadId::new("thread").unwrap(),
        run: workbench_protocol::ids::RunId::new("run").unwrap(),
        incarnation: workbench_protocol::ids::RunId::new("incarnation").unwrap(),
        core_epoch: 17,
        browser_generation: 1,
        page_generation: 2,
    }
}
