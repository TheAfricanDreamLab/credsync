//! Deserialization mirrors that bind wire invariants to decoding.
//!
//! Four wire types carry rules that relate one field to another: an upsert must have a snapshot,
//! a rejection must have a reason, a batch must be `seq`-ordered, a push must fit both its
//! limits. Those rules are useless if a decoder can produce a value that breaks them and simply
//! neglect to call `validate()`.
//!
//! So each of those types deserializes through the mirror struct below and then validates, via
//! `#[serde(try_from = ...)]`. A decoded value is therefore already known to satisfy its rules —
//! the same guarantee the newtypes give for single fields, extended to whole records.
//!
//! # Why mirrors rather than a validating wrapper
//!
//! The field lists are duplicated here, which looks like a drift risk but is not: constructing
//! the real type from its mirror requires every field, so adding a field without updating the
//! mirror fails to compile. The duplication is checked, not trusted.

use crate::error::{ProtocolError, Result};
use crate::ids::{CommandId, EntityId, EntityName, HexString, Reason, ScopeId};
use crate::limits;
use crate::nums::{Cursor, LimitBytes, ProtocolVersion, RowVersion, SchemaVersion, Seq};
use crate::wire::{Batch, Change, Command, CommandResult, Op, PushRequest, Snapshot, Status};
use core::marker::PhantomData;
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// A `Vec` that refuses to grow past `MAX` **while deserializing**, not after.
///
/// `#[serde(try_from = ...)]` builds the entire representation before `TryFrom` runs, so a
/// count check there fires only once every entry has already been parsed and allocated. A client
/// sending a million commands would allocate a million commands in order to be told it sent too
/// many — memory amplification on a protocol whose users are on constrained devices.
///
/// This stops at the limit and then probes for one more element as [`IgnoredAny`], which skips
/// the tokens without constructing `T` or running its validation. So an over-limit request is
/// refused for being over-limit, rather than for whatever happens to be wrong with entry 257.
struct Bounded<T, const MAX: usize>(Vec<T>);

impl<'de, T, const MAX: usize> Deserialize<'de> for Bounded<T, MAX>
where
    T: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        struct V<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T, const MAX: usize> Visitor<'de> for V<T, MAX>
        where
            T: Deserialize<'de>,
        {
            type Value = Vec<T>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "a sequence of at most {MAX} items")
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> core::result::Result<Vec<T>, A::Error> {
                // Capacity is capped at MAX so a hostile size hint cannot pre-allocate.
                let hint = seq.size_hint().unwrap_or(0).min(MAX);
                let mut out = Vec::with_capacity(hint);
                while out.len() < MAX {
                    match seq.next_element::<T>()? {
                        Some(item) => out.push(item),
                        None => return Ok(out),
                    }
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(de::Error::custom(format!(
                        "more than {MAX} items in a bounded collection"
                    )));
                }
                Ok(out)
            }
        }

        d.deserialize_seq(V::<T, MAX>(PhantomData)).map(Bounded)
    }
}

/// Mirror of [`Change`].
#[derive(Deserialize)]
pub(crate) struct ChangeRepr {
    seq: Seq,
    entity: EntityName,
    entity_id: EntityId,
    op: Op,
    #[serde(default)]
    snapshot: Option<Snapshot>,
    row_version: RowVersion,
    schema_version: SchemaVersion,
}

impl TryFrom<ChangeRepr> for Change {
    type Error = ProtocolError;
    fn try_from(r: ChangeRepr) -> Result<Self> {
        let change = Self {
            seq: r.seq,
            entity: r.entity,
            entity_id: r.entity_id,
            op: r.op,
            snapshot: r.snapshot,
            row_version: r.row_version,
            schema_version: r.schema_version,
        };
        change.validate()?;
        Ok(change)
    }
}

/// Mirror of [`Batch`].
#[derive(Deserialize)]
pub(crate) struct BatchRepr {
    scope: ScopeId,
    changes: Vec<Change>,
    next_cursor: Cursor,
    has_more: bool,
    checksum: HexString,
    digest: HexString,
}

impl TryFrom<BatchRepr> for Batch {
    type Error = ProtocolError;
    fn try_from(r: BatchRepr) -> Result<Self> {
        let batch = Self {
            scope: r.scope,
            changes: r.changes,
            next_cursor: r.next_cursor,
            has_more: r.has_more,
            checksum: r.checksum,
            digest: r.digest,
        };
        batch.validate_ordering()?;
        Ok(batch)
    }
}

/// Mirror of [`CommandResult`].
#[derive(Deserialize)]
pub(crate) struct CommandResultRepr {
    id: CommandId,
    status: Status,
    #[serde(default)]
    reason: Option<Reason>,
    #[serde(default)]
    server_seq: Option<Seq>,
}

impl TryFrom<CommandResultRepr> for CommandResult {
    type Error = ProtocolError;
    fn try_from(r: CommandResultRepr) -> Result<Self> {
        let result = Self {
            id: r.id,
            status: r.status,
            reason: r.reason,
            server_seq: r.server_seq,
        };
        result.validate()?;
        Ok(result)
    }
}

/// Mirror of [`PushRequest`].
#[derive(Deserialize)]
pub(crate) struct PushRequestRepr {
    protocol: ProtocolVersion,
    commands: Bounded<Command, { limits::COMMANDS_MAX_COUNT }>,
}

impl TryFrom<PushRequestRepr> for PushRequest {
    type Error = ProtocolError;
    fn try_from(r: PushRequestRepr) -> Result<Self> {
        let request = Self {
            protocol: r.protocol,
            commands: r.commands.0,
        };
        // Still runs: the count is already bounded, but the 1 MB total-bytes limit is not.
        request.validate()?;
        Ok(request)
    }
}

/// Mirror of [`crate::PullRequest`], which bounds its scope list.
#[derive(Deserialize)]
pub(crate) struct PullRequestRepr {
    protocol: ProtocolVersion,
    scopes: Bounded<crate::wire::ScopeCursor, { limits::SCOPES_MAX_COUNT }>,
    #[serde(default)]
    limit_bytes: Option<LimitBytes>,
}

impl TryFrom<PullRequestRepr> for crate::PullRequest {
    type Error = ProtocolError;
    fn try_from(r: PullRequestRepr) -> Result<Self> {
        let request = Self {
            protocol: r.protocol,
            scopes: r.scopes.0,
            limit_bytes: r.limit_bytes,
        };
        // Kept so the programmatic path is checked too; deserialization already bounded it.
        request.validate()?;
        Ok(request)
    }
}
