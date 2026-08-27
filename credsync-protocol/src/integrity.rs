//! Batch checksums and the order-independent scope digest. `docs/spec.md` §5.
//!
//! Two mechanisms with different jobs, and deliberately different algorithms.
//!
//! # Checksums — is this batch intact?
//!
//! A checksum over a batch's canonical encoding catches corruption in transit, in a flaky proxy
//! cache, or in device flash, **before apply**. This is a corruption detector on a link already
//! protected by TLS, so speed matters more than collision resistance: [`checksum`] uses xxh3.
//!
//! # Command checksums — is this the body that was originally sent?
//!
//! Different job. A replay whose body has been mutated must be refused rather than deduped as a
//! success, otherwise the dedupe table becomes a way to launder tampered commands. That is an
//! adversarial question, not a corruption one, so [`payload_checksum`] uses BLAKE3. Design v2.1
//! §5.1 draws this exact line: "xxh3 for speed; BLAKE3 where tamper-evidence matters."
//!
//! # The scope digest — do both sides hold the same rows?
//!
//! [`ScopeDigest`] is an order-independent, incremental hash over `(entity, entity_id,
//! row_version)` for every live row in a scope. Both sides maintain it; the server returns its
//! value with every pull and the client compares after apply. A mismatch means silent
//! divergence — the class of bug that testing missed and users never report until trust is gone.
//!
//! ## Why addition rather than XOR
//!
//! The digest is a sum of per-row hashes in wrapping `u128` arithmetic. Addition is commutative,
//! so order cannot matter, and it has an exact inverse, so removing a row restores the digest to
//! precisely what it was before that row existed — which is what a tombstone must do.
//!
//! XOR has both of those properties too and is the obvious first choice, but it is wrong here:
//! XOR is self-inverse, so **two identical rows cancel out**. A bug that duplicated a row would
//! produce a digest identical to one where the row is absent, and the mechanism whose entire
//! purpose is noticing that state has silently diverged would report agreement. Addition doubles
//! instead of cancelling.

use crate::canonical;
use crate::error::Result;
use crate::ids::{EntityId, EntityName, HexString};
use crate::nums::RowVersion;
use serde::Serialize;
use twox_hash::XxHash3_128;

/// Checksum over a value's canonical encoding, for corruption detection.
///
/// # Errors
/// Returns an error if the value cannot be encoded.
pub fn checksum<T: Serialize>(value: &T) -> Result<HexString> {
    let bytes = canonical::to_vec(value)?;
    Ok(checksum_bytes(&bytes))
}

/// Checksum over raw bytes, for corruption detection.
#[must_use]
pub fn checksum_bytes(bytes: &[u8]) -> HexString {
    hex_u128(XxHash3_128::oneshot(bytes))
}

/// Tamper-evident checksum over a command payload's canonical encoding.
///
/// BLAKE3 rather than xxh3, truncated to 128 bits. The job here is refusing a *mutated* replay,
/// which a non-cryptographic hash cannot promise: xxh3 collisions are findable by construction,
/// and a found collision would let a modified body be accepted as a replay of the original.
///
/// # Errors
/// Returns an error if the value cannot be encoded.
pub fn payload_checksum<T: Serialize>(value: &T) -> Result<HexString> {
    let bytes = canonical::to_vec(value)?;
    Ok(payload_checksum_bytes(&bytes))
}

/// Tamper-evident checksum over raw bytes.
#[must_use]
pub fn payload_checksum_bytes(bytes: &[u8]) -> HexString {
    let full = blake3::hash(bytes);
    let mut truncated = [0u8; 16];
    truncated.copy_from_slice(&full.as_bytes()[..16]);
    hex_u128(u128::from_be_bytes(truncated))
}

/// Formats 128 bits as 32 lowercase hex characters, matching the `HexString` contract.
fn hex_u128(v: u128) -> HexString {
    let mut s = String::with_capacity(32);
    for byte in v.to_be_bytes() {
        // Written by hand rather than with `format!` so the width is unconditional: a leading
        // zero byte must still occupy two characters, or two different digests could render
        // identically.
        s.push(nibble(byte >> 4));
        s.push(nibble(byte & 0x0f));
    }
    HexString::new(s).unwrap_or_else(|_| {
        // Unreachable: 32 lowercase hex characters always satisfy HexString. Constructed
        // defensively rather than with `expect`, which workspace lints deny.
        HexString::new("00").unwrap_or_else(|_| unreachable!("literal is valid hex"))
    })
}

const fn nibble(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + v - 10) as char,
    }
}

/// The per-row contribution to a scope digest.
///
/// Fields are length-prefixed rather than simply concatenated. Without that,
/// `("lesson", "a1")` and `("lessona", "1")` would hash identically, and two genuinely different
/// scopes would agree on a digest.
fn row_hash(entity: &EntityName, entity_id: &EntityId, row_version: RowVersion) -> u128 {
    let e = entity.as_str().as_bytes();
    let i = entity_id.as_str().as_bytes();
    let mut buf = Vec::with_capacity(e.len() + i.len() + 24);
    buf.extend_from_slice(&(e.len() as u64).to_be_bytes());
    buf.extend_from_slice(e);
    buf.extend_from_slice(&(i.len() as u64).to_be_bytes());
    buf.extend_from_slice(i);
    buf.extend_from_slice(&row_version.get().to_be_bytes());
    XxHash3_128::oneshot(&buf)
}

/// An order-independent, incremental digest over the live rows of one scope.
///
/// `docs/spec.md` §5. Both sides maintain one per scope; a mismatch after apply means silent
/// divergence and triggers a tainted-scope re-bootstrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScopeDigest(u128);

impl ScopeDigest {
    /// The digest of a scope holding no rows.
    pub const EMPTY: Self = Self(0);

    /// Adds a live row.
    pub fn add(&mut self, entity: &EntityName, entity_id: &EntityId, row_version: RowVersion) {
        self.0 = self
            .0
            .wrapping_add(row_hash(entity, entity_id, row_version));
    }

    /// Removes a row, restoring the digest to exactly what it was before the row was added.
    ///
    /// This is what makes a tombstone correct: a deleted row must leave the digest as though it
    /// had never existed, not merely "close to" it.
    pub fn remove(&mut self, entity: &EntityName, entity_id: &EntityId, row_version: RowVersion) {
        self.0 = self
            .0
            .wrapping_sub(row_hash(entity, entity_id, row_version));
    }

    /// Replaces a row at one version with the same row at another.
    ///
    /// Equivalent to [`remove`](Self::remove) followed by [`add`](Self::add), and provided so the
    /// two cannot be accidentally separated by a fallible step in between.
    pub fn update(
        &mut self,
        entity: &EntityName,
        entity_id: &EntityId,
        from: RowVersion,
        to: RowVersion,
    ) {
        self.remove(entity, entity_id, from);
        self.add(entity, entity_id, to);
    }

    /// Computes a digest from scratch over a set of live rows.
    ///
    /// Order of the input is irrelevant by construction.
    pub fn from_rows<'a, I>(rows: I) -> Self
    where
        I: IntoIterator<Item = (&'a EntityName, &'a EntityId, RowVersion)>,
    {
        let mut digest = Self::EMPTY;
        for (entity, entity_id, row_version) in rows {
            digest.add(entity, entity_id, row_version);
        }
        digest
    }

    /// The wire representation: 32 lowercase hex characters.
    #[must_use]
    pub fn to_hex(self) -> HexString {
        hex_u128(self.0)
    }

    /// The raw accumulator, for tests and telemetry.
    #[must_use]
    pub const fn raw(self) -> u128 {
        self.0
    }
}
