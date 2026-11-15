//! Opaque identifiers. The core allocates every one of them; the helper only
//! echoes them back, so none of them says anything about runs, threads or
//! storage.

use serde::{Deserialize, Serialize};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id!(
    /// Correlates one request with its response. Chosen by the client.
    RequestId
);
id!(
    /// A group of tabs the core releases together (for example when a run ends).
    SessionId
);
id!(
    /// One page. Never reused within a helper's lifetime.
    TabId
);
id!(
    /// One helper process incarnation. A message from another generation is
    /// stale and is discarded, never acted on.
    RendererGeneration
);
