//! Protocol errors.
//!
//! Every failure to decode is a value, never a panic. A panic in a wire decoder is a remote
//! crash: the network will eventually hand this crate garbage, and it must refuse it politely.

use core::fmt;

/// Everything that can go wrong decoding or validating a wire value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A string field exceeded its declared byte limit.
    TooLong {
        /// The field that overflowed.
        field: &'static str,
        /// Its declared limit in bytes.
        limit: usize,
        /// The length actually supplied.
        actual: usize,
    },
    /// A string field contained a character outside its declared set.
    BadCharset {
        /// The field at fault.
        field: &'static str,
        /// A description of what was expected.
        expected: &'static str,
    },
    /// A field was empty where the specification requires content.
    Empty {
        /// The field at fault.
        field: &'static str,
    },
    /// A numeric field fell outside its declared range.
    OutOfRange {
        /// The field at fault.
        field: &'static str,
        /// The lowest legal value.
        min: u64,
        /// The highest legal value.
        max: u64,
        /// The value actually supplied.
        actual: u64,
    },
    /// A command id was not a hyphenated lowercase UUID.
    MalformedUuid,
    /// A command id parsed but was not version 7.
    NotUuidV7 {
        /// The version nibble found.
        found: u8,
    },
    /// A collection exceeded its declared entry count.
    TooManyItems {
        /// The field at fault.
        field: &'static str,
        /// The declared maximum.
        limit: usize,
        /// The count actually supplied.
        actual: usize,
    },
    /// `snapshot` or `payload` must be a JSON object, not a scalar or array.
    NotAnObject {
        /// The field at fault.
        field: &'static str,
    },
    /// A host document contained a floating-point number, which breaks canonical encoding.
    FloatNotAllowed {
        /// The field at fault.
        field: &'static str,
    },
    /// A rule that ties two fields together was violated.
    Inconsistent {
        /// What was expected.
        detail: &'static str,
    },
    /// The bytes were not well-formed JSON, or did not match the expected shape.
    Malformed {
        /// The underlying parser's description.
        detail: String,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong {
                field,
                limit,
                actual,
            } => write!(f, "{field}: {actual} bytes exceeds the {limit}-byte limit"),
            Self::BadCharset { field, expected } => {
                write!(f, "{field}: expected {expected}")
            }
            Self::Empty { field } => write!(f, "{field}: must not be empty"),
            Self::OutOfRange {
                field,
                min,
                max,
                actual,
            } => write!(f, "{field}: {actual} is outside {min}..={max}"),
            Self::MalformedUuid => write!(f, "command id: not a hyphenated lowercase UUID"),
            Self::NotUuidV7 { found } => {
                write!(
                    f,
                    "command id: expected UUID version 7, found version {found}"
                )
            }
            Self::TooManyItems {
                field,
                limit,
                actual,
            } => write!(f, "{field}: {actual} items exceeds the limit of {limit}"),
            Self::NotAnObject { field } => write!(f, "{field}: must be a JSON object"),
            Self::FloatNotAllowed { field } => write!(
                f,
                "{field}: floating-point numbers break canonical encoding and therefore scope \
digests; use a scaled integer or a decimal string"
            ),
            Self::Inconsistent { detail } => write!(f, "inconsistent: {detail}"),
            Self::Malformed { detail } => write!(f, "malformed: {detail}"),
        }
    }
}

impl core::error::Error for ProtocolError {}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        Self::Malformed {
            detail: e.to_string(),
        }
    }
}

/// Result alias for protocol operations.
pub type Result<T> = core::result::Result<T, ProtocolError>;
