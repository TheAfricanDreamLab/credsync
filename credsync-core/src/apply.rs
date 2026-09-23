//! The pull apply path: cursor walk, ordered apply, digest maintenance.
//!
//! `docs/spec.md` §4 in one sentence: changes within a scope are strictly `seq`-ordered, clients
//! apply them in order, and **the cursor is persisted in the same storage transaction as the rows
//! it covers**.
//!
//! That last clause is the whole reason this module is shaped the way it is. Every row, the new
//! cursor, and the new digest go into one `Vec<StorageOp>` and one [`Storage::transact`] call. A
//! process killed at any point either committed all of it or none of it.
//!
//! Split them and a kill in between leaves a client whose cursor claims changes it never applied.
//! It will never ask for them again, the rows are gone silently and permanently, and every
//! subsequent sync reports success. That is the precise failure this engine exists to make
//! impossible.
//!
//! # What ordering checks can and cannot catch
//!
//! `seq` is a Postgres `bigserial` on `sync_changes`, shared by **every scope**. A single scope's
//! entries are therefore sparse — `1, 2, 7, 19` is a perfectly healthy scope whose neighbours were
//! busy. So a client cannot detect a *missing middle* entry by arithmetic; there is no
//! contiguity to check against.
//!
//! What it can check is that the server never sends it backwards: changes strictly increasing,
//! nothing at or below the cursor, and a `next_cursor` that actually covers what was sent. Those
//! are the violations this module rejects.
//!
//! **The scope digest is what catches the rest**, and this is precisely why it exists. A change
//! silently dropped in the middle leaves the client holding a row set the server does not have,
//! the digests disagree, and the scope is re-bootstrapped. Ordering arithmetic and the digest
//! cover different halves of the same problem; neither is redundant.

use crate::effect::Telemetry;
use crate::storage::StorageOp;
use crate::traits::Storage;
use core::fmt;
use credsync_protocol::{
    Batch, Change, ConflictClass, EntityId, EntityName, Op, RowVersion, ScopeDigest,
};
use std::collections::BTreeMap;

use crate::error::StorageError;

/// Why a batch was refused.
///
/// Refusal is total. Nothing is written and no in-memory state moves, so a rejected batch leaves
/// the client exactly where it was and safe to refetch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApplyError {
    /// Two changes were out of order, or repeated a `seq`.
    ///
    /// `docs/spec.md` §4 requires strict ordering within a scope. A repeat is included here
    /// rather than tolerated: applying the same `seq` twice would add its row contribution to the
    /// digest twice, and the digest is deliberately *not* self-cancelling on duplicates (D-032),
    /// so the damage would persist and be reported as divergence later, far from its cause.
    OutOfOrder {
        /// The `seq` preceding the offender.
        previous: u64,
        /// The offending `seq`.
        found: u64,
    },

    /// A change at or below the cursor — work this client has already done.
    ///
    /// A replayed batch after a flaky retry is the ordinary cause. Refusing rather than silently
    /// skipping is deliberate: re-applying would double-count the row in the digest, and quietly
    /// ignoring it would hide a server that is genuinely rewinding.
    AlreadyApplied {
        /// Where this client's cursor sits.
        cursor: u64,
        /// The `seq` that was at or below it.
        found: u64,
    },

    /// `next_cursor` does not cover the changes the batch carried.
    ///
    /// Continuing from a cursor lower than the last change applied would re-deliver that change
    /// on the next pull, and the client would refuse it as [`AlreadyApplied`](Self::AlreadyApplied)
    /// forever — a scope wedged permanently, one round trip at a time.
    CursorWouldRegress {
        /// The cursor the batch proposes.
        next_cursor: u64,
        /// The highest `seq` it actually carried.
        last_seq: u64,
    },

    /// A change broke the `op`/`snapshot` rule.
    ///
    /// Decoding enforces this (`credsync-protocol`'s `ChangeRepr`), so a batch off the wire
    /// cannot carry one. A batch built programmatically — in a test, in the simulator, in a
    /// host's own tooling — can, and the apply path must not assume its input came from a decoder.
    Inconsistent {
        /// What was expected.
        detail: &'static str,
    },

    /// An upsert would overwrite an existing row of an append-only entity.
    ///
    /// `docs/spec.md` §6: append-only streams — submissions, attendance events — have *"no
    /// conflict by construction"* because new versions never overwrite. A server sending a second
    /// version of an existing row has broken that promise, and the client says so rather than
    /// quietly accepting it.
    ///
    /// Refusing the whole batch is safe here precisely because refusal writes nothing: the client
    /// stays exactly where it was, so this cannot itself cause the divergence it is reporting.
    AppendOnlyOverwrite {
        /// The entity whose contract was broken.
        entity: EntityName,
        /// The row that would have been overwritten.
        entity_id: EntityId,
        /// The version already stored.
        stored: u64,
        /// The version that arrived.
        incoming: u64,
    },

    /// Storage refused the transaction, so nothing was applied.
    Storage(StorageError),
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfOrder { previous, found } => write!(
                f,
                "changes must strictly increase by seq: {found} followed {previous}"
            ),
            Self::AlreadyApplied { cursor, found } => write!(
                f,
                "seq {found} is at or below the cursor {cursor}; already applied"
            ),
            Self::CursorWouldRegress {
                next_cursor,
                last_seq,
            } => write!(
                f,
                "next_cursor {next_cursor} does not cover the last change applied, seq {last_seq}"
            ),
            Self::AppendOnlyOverwrite {
                entity,
                entity_id,
                stored,
                incoming,
            } => write!(
                f,
                "{entity}/{entity_id} is append-only: version {incoming} would overwrite {stored}"
            ),
            Self::Inconsistent { detail } => write!(f, "inconsistent change: {detail}"),
            Self::Storage(e) => write!(f, "storage refused the batch: {e}"),
        }
    }
}

impl core::error::Error for ApplyError {}

impl From<StorageError> for ApplyError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

/// What one accepted batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Applied {
    /// How many changes were applied.
    pub changes: usize,
    /// Whether the client's digest disagreed with the server's after applying.
    ///
    /// `true` means **silent divergence**: both sides walked the same log and ended up holding
    /// different rows. `docs/spec.md` §5 requires the scope be marked tainted and re-bootstrapped,
    /// which lands at CS-22 (#23). What happens here is the detection and the telemetry — the
    /// batch itself is still committed, because refusing it would strand the client at a cursor
    /// it can never advance past while leaving the divergence exactly as unresolved.
    pub diverged: bool,
}

/// Checks every ordering rule before a single byte is written.
///
/// Validation is complete before any storage op is built, so a batch that will be refused is
/// refused without touching the database at all.
pub(crate) fn validate_ordering(batch: &Batch, cursor: u64) -> Result<(), ApplyError> {
    let mut previous: Option<u64> = None;

    for change in &batch.changes {
        let seq = change.seq.get();

        if seq <= cursor {
            return Err(ApplyError::AlreadyApplied { cursor, found: seq });
        }
        if let Some(prev) = previous
            && seq <= prev
        {
            return Err(ApplyError::OutOfOrder {
                previous: prev,
                found: seq,
            });
        }
        change
            .validate()
            .map_err(|_| match (change.op, change.snapshot.is_some()) {
                (Op::Upsert, false) => ApplyError::Inconsistent {
                    detail: "op=upsert requires a snapshot",
                },
                _ => ApplyError::Inconsistent {
                    detail: "op=delete must not carry a snapshot",
                },
            })?;

        previous = Some(seq);
    }

    if let Some(last_seq) = previous
        && batch.next_cursor.get() < last_seq
    {
        return Err(ApplyError::CursorWouldRegress {
            next_cursor: batch.next_cursor.get(),
            last_seq,
        });
    }

    Ok(())
}

/// What a row's version will be once the batch commits: `None` means "deleted by this batch".
///
/// Keyed by `(entity, entity_id)`, in a `BTreeMap` rather than a `HashMap` so nothing about this
/// depends on a per-process hash seed.
pub(crate) type Staged = BTreeMap<(EntityName, EntityId), Option<RowVersion>>;

/// Builds the storage ops for one change and folds it into the running digest.
///
/// The digest arithmetic is the delicate part. For each change the row's *current* version is
/// needed first, because the contribution being replaced or removed must be subtracted at the
/// version actually held — not at the version the change mentions, which for an upsert is the new
/// one and for a tombstone is meaningless.
///
/// # Why `staged` exists
///
/// "Currently held" cannot be read from storage alone. A batch is applied as one transaction, so
/// an earlier change in the *same* batch has not committed yet and is invisible to
/// [`Storage::row_version`]. A row created and then deleted within one batch — routine, since a
/// pull window can span days for an offline device — would therefore be added to the digest and
/// never subtracted, because the tombstone would look up a row storage had never heard of.
///
/// The client would then hold a digest describing a row it does not have, disagree with the
/// server on every subsequent pull, and re-bootstrap the scope forever: the divergence detector
/// firing on damage it caused itself.
///
/// Found by `tests/apply_property.rs` on a two-change sequence, not by review.
pub(crate) fn stage_change<S: Storage>(
    storage: &S,
    change: &Change,
    class: Option<ConflictClass>,
    digest: &mut ScopeDigest,
    ops: &mut Vec<StorageOp>,
    staged: &mut Staged,
) -> Result<(), ApplyError> {
    let key = (change.entity.clone(), change.entity_id.clone());

    // What this batch has already decided wins over what storage still says.
    let current = match staged.get(&key) {
        Some(pending) => *pending,
        None => storage.row_version(&change.entity, &change.entity_id)?,
    };

    match change.op {
        Op::Upsert => {
            let Some(snapshot) = change.snapshot.clone() else {
                return Err(ApplyError::Inconsistent {
                    detail: "op=upsert requires a snapshot",
                });
            };

            // `docs/spec.md` §6: an append-only stream's entries are never replaced. A second
            // version of a row that already exists means the server broke that contract, which is
            // worth refusing loudly rather than absorbing. Refusal is safe here precisely because
            // it writes nothing — the client stays where it was, so this cannot itself cause the
            // divergence it is reporting.
            if let (Some(ConflictClass::AppendOnly), Some(old)) = (class, current)
                && old != change.row_version
            {
                return Err(ApplyError::AppendOnlyOverwrite {
                    entity: change.entity.clone(),
                    entity_id: change.entity_id.clone(),
                    stored: old.get(),
                    incoming: change.row_version.get(),
                });
            }

            match current {
                // Replacing a row: subtract the old contribution, add the new one.
                Some(old) => {
                    digest.update(&change.entity, &change.entity_id, old, change.row_version)
                }
                // A row this client has never held.
                None => digest.add(&change.entity, &change.entity_id, change.row_version),
            }

            ops.push(StorageOp::UpsertRow {
                entity: change.entity.clone(),
                entity_id: change.entity_id.clone(),
                snapshot,
                row_version: change.row_version,
                schema_version: change.schema_version,
            });
            staged.insert(key, Some(change.row_version));
        }

        Op::Delete => {
            // A tombstone must leave the digest exactly as though the row had never existed
            // (`docs/spec.md` §5). Subtract at the stored version, which is the only version that
            // was ever added.
            //
            // A tombstone for a row this client does not hold is not an error. The row may have
            // been created and deleted entirely within one pull window, or this client may never
            // have been sent it. Deleting nothing changes nothing, and the digest must not move --
            // subtracting a contribution that was never added would corrupt it permanently.
            if let Some(old) = current {
                digest.remove(&change.entity, &change.entity_id, old);
            }

            ops.push(StorageOp::DeleteRow {
                entity: change.entity.clone(),
                entity_id: change.entity_id.clone(),
            });
            staged.insert(key, None);
        }
    }

    Ok(())
}

/// Builds the divergence report for a scope whose digests disagree.
pub(crate) fn divergence(batch: &Batch, client: &ScopeDigest) -> Telemetry {
    Telemetry::ScopeDiverged {
        scope: batch.scope.clone(),
        client: client.to_hex(),
        server: batch.digest.clone(),
    }
}
