//! Worker C: MCP-only destination-bound private/LAN exceptions.
//! Existing NetworkGuard::post_json/get policy is intentionally unchanged.
use std::fmt;
/// Only a checked destination validator in this module may construct this value.
/// No public constructor/Deserialize/Serialize; not reusable by web tools.
pub struct ApprovedMcpDestination {
    _validated: (),
}
impl fmt::Debug for ApprovedMcpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovedMcpDestination(<opaque>)")
    }
}
