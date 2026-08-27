//! Validated numeric newtypes.
//!
//! Ranges come from `docs/spec.md` §2.1. As with the identifier types, deserialization routes
//! through the same constructor, so an out-of-range value cannot enter through the wire path.

use crate::error::{ProtocolError, Result};
use crate::limits;
use serde::{Deserialize, Serialize};

macro_rules! bounded_num {
    ($name:ident, $inner:ty, $inner_str:literal, $field:literal, $min:expr, $max:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = $inner_str, into = $inner_str)]
        pub struct $name($inner);

        impl $name {
            #[doc = concat!("Validates and wraps a `", $field, "`.")]
            ///
            /// # Errors
            /// Returns [`ProtocolError::OutOfRange`] if the value falls outside its declared range.
            pub const fn new(v: $inner) -> Result<Self> {
                if v < $min || v > $max {
                    return Err(ProtocolError::OutOfRange {
                        field: $field,
                        min: $min as u64,
                        max: $max as u64,
                        actual: v as u64,
                    });
                }
                Ok(Self(v))
            }

            /// The underlying value.
            #[must_use]
            pub const fn get(self) -> $inner {
                self.0
            }
        }

        impl TryFrom<$inner> for $name {
            type Error = ProtocolError;
            fn try_from(v: $inner) -> Result<Self> {
                Self::new(v)
            }
        }

        impl From<$name> for $inner {
            fn from(v: $name) -> Self {
                v.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

bounded_num!(
    ProtocolVersion,
    u16,
    "u16",
    "protocol",
    limits::PROTOCOL_VERSION_MIN,
    u16::MAX,
    "Wire protocol version. `docs/spec.md` §7: the server speaks N and N-1."
);
bounded_num!(
    SchemaVersion,
    u16,
    "u16",
    "schema_version",
    limits::SCHEMA_VERSION_MIN,
    u16::MAX,
    "Per-entity schema version carried in every snapshot. `docs/spec.md` §7."
);
bounded_num!(
    Seq,
    u64,
    "u64",
    "seq",
    limits::SEQ_MIN,
    limits::SEQ_MAX,
    "A change-log position. Capped at `i64::MAX` because `seq` is a Postgres `bigserial`."
);
bounded_num!(
    RowVersion,
    u64,
    "u64",
    "row_version",
    limits::SEQ_MIN,
    limits::SEQ_MAX,
    "Server-assigned row version. `docs/spec.md` §6: this, not `client_ts`, decides LWW."
);
bounded_num!(
    LimitBytes,
    u32,
    "u32",
    "limit_bytes",
    limits::LIMIT_BYTES_MIN,
    limits::LIMIT_BYTES_MAX,
    "Client hint for batch size. The server may return less, never more."
);

/// A client cursor: a change-log position, or [`Cursor::START`] meaning "from the beginning".
///
/// Distinct from [`Seq`] because zero is legal here and nowhere else. Conflating the two is how
/// an off-by-one silently becomes a skipped change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Cursor(u64);

impl Cursor {
    /// A client that has never synced this scope.
    pub const START: Self = Self(limits::CURSOR_START);

    /// Validates and wraps a cursor.
    ///
    /// # Errors
    /// Returns [`ProtocolError::OutOfRange`] above `2^63-1`.
    pub const fn new(v: u64) -> Result<Self> {
        if v > limits::SEQ_MAX {
            return Err(ProtocolError::OutOfRange {
                field: "cursor",
                min: limits::CURSOR_START,
                max: limits::SEQ_MAX,
                actual: v,
            });
        }
        Ok(Self(v))
    }

    /// The underlying value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for Cursor {
    type Error = ProtocolError;
    fn try_from(v: u64) -> Result<Self> {
        Self::new(v)
    }
}

impl From<Cursor> for u64 {
    fn from(v: Cursor) -> Self {
        v.0
    }
}

impl core::fmt::Display for Cursor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
