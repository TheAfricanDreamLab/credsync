//! Wire types, canonical codec, checksums and scope digests for the credSync protocol.
//!
//! This crate is the single definition of what goes on the wire. [`docs/spec.md`] is its
//! specification and takes precedence: any wire change updates the spec, the types, the codec
//! and the fixtures **in the same pull request**, never separately.
//!
//! # Canonical encoding
//!
//! Encoding is *canonical*: the same logical value always produces identical bytes. This is
//! load-bearing rather than cosmetic, because checksums and scope digests are computed over the
//! encoding — instability there manifests as phantom corruption reports on real devices. See
//! [`canonical`] for how that stability is achieved and why it does not depend on struct field
//! order.
//!
//! # Validation
//!
//! Every bounded field is a newtype that can only be constructed through validation, and
//! deserialization routes through the same constructor. An invalid value is therefore
//! unrepresentable rather than merely discouraged, and the wire path cannot drift from the
//! programmatic one.
//!
//! Oversize input is **rejected, never truncated**, and no input — however malformed or
//! truncated — causes a panic. A panic in a wire decoder is a remote crash.
//!
//! # Status
//!
//! Types and codec landed at CS-3, checksums and digests at CS-4. Golden fixtures arrive at CS-5.
//!
//! [`docs/spec.md`]: https://github.com/TheAfricanDreamLab/credsync/blob/main/docs/spec.md

#![forbid(unsafe_code)]

pub mod canonical;
pub mod error;
pub mod ids;
pub mod integrity;
pub mod limits;
pub mod nums;
mod validated;
pub mod wire;

pub use error::{ProtocolError, Result};
pub use ids::{CommandId, CommandName, EntityId, EntityName, HexString, Reason, ScopeId};
pub use integrity::{
    ScopeDigest, checksum, checksum_bytes, payload_checksum, payload_checksum_bytes,
};
pub use nums::{Cursor, LimitBytes, ProtocolVersion, RowVersion, SchemaVersion, Seq};
pub use wire::{
    Batch, BootstrapRequest, BootstrapResponse, BootstrapRow, Change, Command, CommandResult,
    ConflictClass, Document, EntityRegistration, ForcedUpgrade, Op, Payload, PullRequest,
    PullResponse, PushRequest, PushResponse, ScopeCursor, Snapshot, Status,
};

/// The protocol version this crate implements.
pub const PROTOCOL_V1: u16 = 1;
