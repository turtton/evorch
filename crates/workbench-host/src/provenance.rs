//! Provenance the host attaches to what it takes in.
//!
//! The protocol says provenance is added by the core's ingress and never sent
//! by a client ([`workbench_protocol::envelope::Provenance`]). This module has
//! the values the in-process paths use. Provenance is bookkeeping kept beside
//! commands in [`crate::receipts`]; it is never written into a prompt or a run
//! config, so it cannot change the model input or the prompt cache.

use workbench_protocol::envelope::{Provenance, Surface};
use workbench_protocol::ids::ClientId;

/// A human acting through the desktop GUI of this process.
///
/// The client id is derived from the process, so it names this window's
/// process and nothing about the machine or its network.
pub fn local_desktop() -> Provenance {
    let id = format!("desktop-{}", std::process::id());
    let client = ClientId::new(id).expect("a process id forms a valid client id");
    Provenance::human(Surface::Desktop, client)
}
