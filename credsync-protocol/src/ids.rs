//! Validated identifier newtypes.
//!
//! Each type can only be constructed through validation, so an invalid value is unrepresentable
//! rather than merely undesirable. Deserialization routes through the same constructor via
//! `#[serde(try_from = ...)]`, which means the wire path and the programmatic path cannot drift.

use crate::error::{ProtocolError, Result};
use crate::limits;
use serde::{Deserialize, Serialize};

/// Validates length and character set for a bounded string field.
fn checked(
    field: &'static str,
    expected: &'static str,
    max: usize,
    ok: fn(u8) -> bool,
    s: &str,
) -> Result<()> {
    if s.is_empty() {
        return Err(ProtocolError::Empty { field });
    }
    // Byte length, not char count: the limit in spec.md is expressed in bytes, and a multi-byte
    // character must not smuggle a value past a byte budget.
    if s.len() > max {
        return Err(ProtocolError::TooLong {
            field,
            limit: max,
            actual: s.len(),
        });
    }
    if !s.bytes().all(ok) {
        return Err(ProtocolError::BadCharset { field, expected });
    }
    Ok(())
}

const fn is_scope_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-')
}

const fn is_lower_snake(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

const fn is_entity_id_byte(b: u8) -> bool {
    // entity_id carries host-chosen keys, so the charset is wider than entity names, but control
    // characters and whitespace stay out: they break URLs, logs, and eyeballs alike.
    b.is_ascii_graphic()
}

macro_rules! bounded_string {
    ($name:ident, $field:literal, $max:expr, $expected:literal, $pred:path) => {
        #[doc = concat!("Validated `", $field, "`. See `docs/spec.md` §2.1.")]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            #[doc = concat!("Validates and wraps a `", $field, "`.")]
            ///
            /// # Errors
            /// Returns an error if the value is empty, exceeds its byte limit, or contains a
            /// character outside the declared set.
            pub fn new(s: impl Into<String>) -> Result<Self> {
                let s = s.into();
                checked($field, $expected, $max, $pred, &s)?;
                Ok(Self(s))
            }

            /// Borrows the underlying string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProtocolError;
            fn try_from(s: String) -> Result<Self> {
                Self::new(s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> Self {
                v.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

bounded_string!(
    ScopeId,
    "scope",
    limits::SCOPE_MAX_BYTES,
    "ASCII alphanumeric, ':', '_' or '-'",
    is_scope_byte
);
bounded_string!(
    EntityName,
    "entity",
    limits::ENTITY_MAX_BYTES,
    "lowercase ASCII, digits or '_'",
    is_lower_snake
);
bounded_string!(
    EntityId,
    "entity_id",
    limits::ENTITY_ID_MAX_BYTES,
    "printable ASCII without spaces",
    is_entity_id_byte
);
bounded_string!(
    CommandName,
    "name",
    limits::COMMAND_NAME_MAX_BYTES,
    "lowercase ASCII, digits or '_'",
    is_lower_snake
);
bounded_string!(
    Reason,
    "reason",
    limits::REASON_MAX_BYTES,
    "printable text",
    is_reason_byte
);

const fn is_reason_byte(b: u8) -> bool {
    // Human-readable text reaches a user interface, so allow spaces and punctuation but keep
    // control characters out — they corrupt logs and terminals.
    b == b' ' || b.is_ascii_graphic()
}

/// A batch checksum or scope digest: lowercase hex, at most 64 characters.
///
/// The exact width is fixed by the algorithm chosen at CS-4 (`DECISIONS.md` O-001), so this
/// type enforces the bound and the alphabet rather than a specific length. An odd number of
/// characters is refused: hex encodes whole bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HexString(String);

impl HexString {
    /// Validates and wraps a lowercase hex string.
    ///
    /// # Errors
    /// Returns an error if the value is empty, longer than 64 characters, contains anything
    /// other than `0-9a-f`, or has an odd length.
    pub fn new(s: impl Into<String>) -> Result<Self> {
        let s = s.into();
        checked(
            "digest",
            "lowercase hex",
            limits::HEX_MAX_CHARS,
            |b| b.is_ascii_digit() || b.is_ascii_lowercase() && b <= b'f',
            &s,
        )?;
        if s.len() % 2 != 0 {
            return Err(ProtocolError::BadCharset {
                field: "digest",
                expected: "an even number of hex characters",
            });
        }
        Ok(Self(s))
    }

    /// Borrows the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HexString {
    type Error = ProtocolError;
    fn try_from(s: String) -> Result<Self> {
        Self::new(s)
    }
}

impl From<HexString> for String {
    fn from(v: HexString) -> Self {
        v.0
    }
}

impl core::fmt::Display for HexString {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A command identifier: a UUID version 7, hyphenated and lowercase on the wire.
///
/// Stored as raw bytes and parsed by hand rather than pulling the `uuid` crate, because its
/// generation features depend on `getrandom` — which is on the sans-IO ban list for
/// `credsync-core`, and `credsync-core` depends on this crate. Parsing it here also lets the
/// version nibble be enforced, which the crate would not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandId([u8; 16]);

const HYPHENS_AT: [usize; 4] = [8, 13, 18, 23];
const UUID_LEN: usize = 36;

impl CommandId {
    /// Wraps raw bytes, checking only that they carry version 7.
    ///
    /// # Errors
    /// Returns an error if the version nibble is not 7.
    pub const fn from_bytes(b: [u8; 16]) -> Result<Self> {
        let version = b[6] >> 4;
        if version != 7 {
            return Err(ProtocolError::NotUuidV7 { found: version });
        }
        Ok(Self(b))
    }

    /// The raw 16 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Parses a hyphenated lowercase UUIDv7.
    ///
    /// # Errors
    /// Returns [`ProtocolError::MalformedUuid`] for anything that is not exactly 36 characters
    /// of lowercase hex with hyphens in the canonical positions, and
    /// [`ProtocolError::NotUuidV7`] if it parses but is not version 7.
    pub fn parse(s: &str) -> Result<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != UUID_LEN {
            return Err(ProtocolError::MalformedUuid);
        }
        let mut out = [0u8; 16];
        let mut out_i = 0usize;
        let mut i = 0usize;
        while i < UUID_LEN {
            if HYPHENS_AT.contains(&i) {
                if bytes[i] != b'-' {
                    return Err(ProtocolError::MalformedUuid);
                }
                i += 1;
                continue;
            }
            // Two hex digits become one byte. A hyphen cannot appear here, because the canonical
            // positions are handled above and any other hyphen fails the nibble check.
            let hi = hex_nibble(bytes[i]).ok_or(ProtocolError::MalformedUuid)?;
            let lo = bytes
                .get(i + 1)
                .copied()
                .and_then(hex_nibble)
                .ok_or(ProtocolError::MalformedUuid)?;
            if out_i >= out.len() {
                return Err(ProtocolError::MalformedUuid);
            }
            out[out_i] = (hi << 4) | lo;
            out_i += 1;
            i += 2;
        }
        if out_i != 16 {
            return Err(ProtocolError::MalformedUuid);
        }
        Self::from_bytes(out)
    }
}

/// Lowercase hex only: an uppercase UUID is a different string and would break byte-stability.
const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

impl core::fmt::Display for CommandId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for CommandId {
    type Error = ProtocolError;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<CommandId> for String {
    fn from(v: CommandId) -> Self {
        v.to_string()
    }
}
