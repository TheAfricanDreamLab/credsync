//! Canonical encoding.
//!
//! Canonical means **byte-stable**: the same logical value always produces identical bytes.
//! This is load-bearing rather than cosmetic — batch checksums and scope digests are computed
//! over this encoding (`docs/spec.md` §5), so instability here surfaces as phantom corruption
//! reports on real devices, which is among the worst failure modes this project could ship.
//!
//! # How stability is achieved
//!
//! Encoding goes through [`serde_json::Value`] rather than serializing a struct directly.
//! That matters: `serde` emits struct fields in **declaration order**, so a direct encode would
//! make canonicality depend on the order fields happen to be written in a source file. Someone
//! tidying a struct months from now would silently change every digest.
//!
//! Routing through `Value` removes that coupling. `serde_json::Map` is a `BTreeMap` unless the
//! `preserve_order` feature is enabled — which this crate must never enable — so object keys
//! sort on the way out, recursively, regardless of declaration order.
//!
//! # Numbers
//!
//! Every number the protocol itself defines is an integer, which has exactly one JSON
//! representation. Floats can still appear inside opaque host data in `snapshot` and `payload`;
//! `serde_json` formats those with a shortest-round-trip algorithm that is deterministic for a
//! given value, so re-encoding a decoded document is stable.

use crate::error::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Encodes a value to its canonical bytes.
///
/// # Errors
/// Returns an error if the value cannot be represented as JSON.
pub fn to_vec<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    // Two steps deliberately: to Value first (sorting keys), then to bytes. Serializing `T`
    // directly would emit fields in declaration order and lose canonicality.
    let v = serde_json::to_value(value)?;
    Ok(serde_json::to_vec(&v)?)
}

/// Encodes a value to a canonical string.
///
/// # Errors
/// Returns an error if the value cannot be represented as JSON.
pub fn to_string<T: Serialize>(value: &T) -> Result<String> {
    let v = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&v)?)
}

/// Decodes canonical bytes into a value.
///
/// # Errors
/// Returns [`crate::ProtocolError::Malformed`] for input that is not well-formed JSON or does
/// not match the expected shape, and the specific validation error for a value that parses but
/// breaks a declared limit. Never panics, whatever the input.
pub fn from_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(bytes)?)
}

/// Rewrites arbitrary JSON into canonical form.
///
/// Used for opaque host documents (`snapshot`, `payload`) whose contents this crate does not
/// model but must still encode stably.
///
/// # Errors
/// Returns an error if the value cannot be re-encoded.
pub fn canonicalize(value: &Value) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

/// The canonical byte length of a value, without keeping the bytes.
///
/// # Errors
/// Returns an error if the value cannot be encoded.
pub fn encoded_len<T: Serialize>(value: &T) -> Result<usize> {
    Ok(to_vec(value)?.len())
}
