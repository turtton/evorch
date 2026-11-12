//! Desktop adapter for ending a lane by hand. The provenance is the desktop's own, built by
//! the host crate from this process; nothing the dialog or a lane says can change it.
use crate::app::lane_close::{LaneCloseFailure, LaneCloseHost, LaneCloseOrder};
use event_bus::lane::LaneCloseResult;
use runtime::{AgentRuntime, lane_close::HumanLaneCloseRequest};
use workbench_host::lane_management::HumanLaneManagement;
use workbench_protocol::{envelope::Provenance, errors::ErrorCode};

pub struct GuiLaneCloseHost {
    adapter: HumanLaneManagement,
    runtime: AgentRuntime,
    provenance: Provenance,
}

impl GuiLaneCloseHost {
    pub fn new(runtime: AgentRuntime) -> Self {
        Self {
            adapter: HumanLaneManagement::new(runtime.clone(), None),
            runtime,
            provenance: workbench_host::provenance::local_desktop(),
        }
    }

    pub fn with_ownership(mut self, owner: std::sync::Arc<runtime::ownership::OwnerHost>) -> Self {
        self.adapter = HumanLaneManagement::new(self.runtime.clone(), Some(owner));
        self
    }
}

impl LaneCloseHost for GuiLaneCloseHost {
    fn close(&self, order: LaneCloseOrder) -> Result<LaneCloseResult, LaneCloseFailure> {
        let request = HumanLaneCloseRequest {
            lane_id: order.lane_id,
            ready_seq: order.ready_seq,
            expected_management_revision: order.expected_management_revision,
            expected_request_revision: order.expected_request_revision,
            discard: order.discard,
            note: order.note,
            surface: String::new(),
            client_id: None,
        };
        self.adapter
            .close_lane_as(request, &self.provenance)
            .map_err(|error| LaneCloseFailure {
                stale: error.code == ErrorCode::StaleRevision,
                message: error.message,
            })
    }
}
