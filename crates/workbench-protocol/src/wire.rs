//! Integers that JSON consumers cannot hold exactly.
//!
//! JavaScript numbers lose precision above 2^53, so every `u64` that crosses
//! the wire is a canonical decimal string (`"0"`, `"42"`; no sign, no leading
//! zeros, no whitespace). Narrow counters (`u32`) stay JSON numbers.

use serde::de::{self, Visitor};
use serde::{Deserializer, Serializer};
use std::fmt;

/// Parse the canonical decimal form.
pub fn parse_u64(text: &str) -> Option<u64> {
    let canonical = match text.as_bytes() {
        [] => false,
        [b'0'] => true,
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    if canonical { text.parse().ok() } else { None }
}

pub(crate) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    struct Decimal;
    impl Visitor<'_> for Decimal {
        type Value = u64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a canonical decimal string holding a u64")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            parse_u64(v).ok_or_else(|| E::custom("not a canonical decimal u64 string"))
        }
    }
    deserializer.deserialize_str(Decimal)
}

macro_rules! wire_u64 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default,
                 serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(#[serde(with = "crate::wire")] pub u64);

        impl $name {
            /// The numeric value.
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

wire_u64!(
    /// Opaque equality token identifying an entity/collection state. Thread, run,
    /// receipt and project revisions may be content hashes: NEVER compare them
    /// numerically. Use EventCursor (epoch + sequence) for event ordering.
    Revision
);
wire_u64!(
    /// Position of an event within one core instance epoch.
    Sequence
);
wire_u64!(
    /// Byte offset into a body.
    ByteOffset
);
wire_u64!(
    /// Identifies one run of the core process; changes on every restart.
    InstanceEpoch
);
wire_u64!(
    /// A count of messages, as the runtime publishes it for fork seeds.
    MessageCount
);
wire_u64!(
    /// Milliseconds since the Unix epoch. Display only; never used for ordering.
    UnixMillis
);
wire_u64!(
    /// Client-chosen value echoed by `Pong`.
    Nonce
);
