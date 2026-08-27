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
//! representation.
//!
//! Floats are **refused** inside `snapshot` and `payload` at any depth (`DECISIONS.md` D-028).
//! `serde_json` parsing is not exact — a value can re-parse one ULP away and then re-encode
//! shorter — so a document holding a float does not survive a serialize/parse/serialize cycle
//! unchanged, and two sides holding the same logical row would compute different digests. Hosts
//! use scaled integers or decimal strings. Do not remove that guard: `tests/canonical.rs` pins
//! both the rule and the `serde_json` behaviour that motivates it.

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

/// Counts bytes written and discards them.
///
/// Exists so [`encoded_len`] can measure without materialising the buffer.
struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The canonical byte length of a value, without keeping the bytes.
///
/// Streams into a counting sink rather than building the encoded buffer. That matters on the
/// path this is used for: [`crate::PushRequest::validate`] measures a request against a 1 MB
/// budget, and 256 maximum-size payloads would otherwise allocate roughly 16 MB purely in order
/// to decide to reject them.
///
/// # Errors
/// Returns an error if the value cannot be encoded.
pub fn encoded_len<T: Serialize>(value: &T) -> Result<usize> {
    // Still through `Value` first, so the measured length is the *canonical* length rather than
    // the declaration-order one. See the module docs.
    let v = serde_json::to_value(value)?;
    let mut counter = CountingWriter(0);
    serde_json::to_writer(&mut counter, &v)?;
    Ok(counter.0)
}
