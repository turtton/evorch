//! Agent observation over the core's REAL control and data sockets (O-V1).
//!
//! The port behind the service is a FAKE in every test. Two kinds of core
//! carry it:
//! * `Core::start`: the fake service is installed through a test-only host
//!   method and its authority is permissive. These tests prove the transport
//!   (peer, client id, connection epoch, token, bind, ACK, close, limits).
//! * `Core::start_real`: the PRODUCTION service (stored membership, run phase
//!   and Stop request, notifications through the projection, lifecycle task)
//!   over the fake port, and a REAL run of the real runtime. The
//!   `i705_4_real_*` tests and the notification test of item 1 use it.
//!
//! Neither proves that production wires `AgentObservationSource`.
//!
//! Test names carry the item of issue #705 they verify: `i705_1_*` the normal
//! path, `i705_2_*` refusals, `i705_3_*` disconnects and late callbacks,
//! `i705_4_*` terminal states and generations, `i705_5_*` races, the 30
//! seconds, slow readers and the shared eight places.
//!
//! Not covered here, by construction:
//! * a peer with another UID: it needs a second user, which an unprivileged
//!   test cannot be; the check is one comparison at the top of
//!   `unix::connection`;
//! * a process with the SAME UID that claims another client's id: outside the
//!   authentication model (peer UID + client id + connection epoch);
//! * the Web and Agent provenances of `i705_2_web_agent_cli_and_unknown_*`
//!   reach the host's dispatch directly (`HostBackend::request`), not through
//!   a socket: a client cannot name its surface or actor on the wire;
//! * the exact 29.999 / 30.000 second boundary of the registry window: the
//!   service's own test pins it; a paused clock with real sockets jumps to the
//!   next timer whenever the runtime waits for a socket.
#![cfg(target_os = "linux")]

#[path = "agent_observation_transport/harness.rs"]
mod harness;
#[path = "agent_observation_transport/i705_1_normal.rs"]
mod i705_1_normal;
#[path = "agent_observation_transport/i705_2_refusals.rs"]
mod i705_2_refusals;
#[path = "agent_observation_transport/i705_3_disconnect.rs"]
mod i705_3_disconnect;
#[path = "agent_observation_transport/i705_4_terminal.rs"]
mod i705_4_terminal;
#[path = "agent_observation_transport/i705_5_limits.rs"]
mod i705_5_limits;
