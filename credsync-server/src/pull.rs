//! Pull pagination over the change log. `docs/spec.md` §3.2.
//!
//! # The budget is compressed bytes, never a row count
//!
//! Platform Plan v1.1 §7.4 and `docs/spec.md` §2 are specific: byte budgets are **compressed-size**
//! budgets, default 100 KB. A hundred rows of large snapshots must not blow a 2G connection's
//! budget just because a hundred sounded reasonable.
//!
//! So the server fills a batch by measuring, not by counting. Each change is appended, the batch
//! is encoded canonically, and the compressed size is checked — the same discipline the client
//! uses to fill a push (D-042), for the same reason.
//!
//! # One change always goes, even alone over budget
//!
//! `docs/spec.md` §2: *"a single change larger than the budget is still delivered alone rather
//! than stalling the cursor."* A snapshot can be up to 256 KB and the default budget is 100 KB, so
//! this is not a corner case — it is a Tuesday. Refusing to send it would wedge the scope
//! permanently: the client asks, the server has something it will not send, and the cursor never
//! moves again.

use crate::error::ServerError;
use credsync_protocol::{
    Batch, Change, Cursor, EntityId, EntityName, HexString, Op, RowVersion, SchemaVersion, ScopeId,
    Seq, Snapshot, canonical,
};

/// Measures how large a batch will be on the wire.
///
/// The same trait shape the client uses, and for the same reason: `docs/spec.md` §2 negotiates
/// Brotli or gzip, so neither side can know the algorithm in advance and neither should guess.
pub trait Compressor {
    /// The length `bytes` would occupy compressed.
    fn compressed_len(&self, bytes: &[u8]) -> usize;
}

/// The default budget, `docs/spec.md` §2.
pub const DEFAULT_BUDGET_BYTES: usize = 100_000;

/// One row of `sync_changes`, as read from the database.
///
/// Deliberately not [`Change`]: the database row is untrusted until validated. Every field goes
/// through the protocol's newtypes on the way out, so a scope name that somehow got into the table
/// without passing validation is refused here rather than put on the wire.
#[derive(Debug, Clone)]
pub struct ChangeRow {
    /// Position in the log.
    pub seq: i64,
    /// Which entity the row belongs to.
    pub entity: String,
    /// The row's identifier.
    pub entity_id: String,
    /// `upsert` or `delete`.
    pub op: String,
    /// The full row, absent for a tombstone.
    pub snapshot: Option<serde_json::Value>,
    /// Server-assigned row version.
    pub row_version: i64,
    /// The schema this snapshot was written under.
    pub schema_version: i32,
}

impl ChangeRow {
    /// Converts a database row into a validated wire value.
    ///
    /// # Errors
    /// Returns [`ServerError::Corrupt`] if any field fails the protocol's own validation. That is
    /// a database holding something the protocol says cannot exist, which is worth refusing
    /// loudly rather than forwarding to every client that asks.
    pub fn into_change(self) -> Result<Change, ServerError> {
        let op = match self.op.as_str() {
            "upsert" => Op::Upsert,
            "delete" => Op::Delete,
            other => {
                return Err(ServerError::Corrupt {
                    detail: format!("sync_changes.op is '{other}', which is not a valid op"),
                });
            }
        };

        let snapshot = match (op, self.snapshot) {
            (Op::Upsert, Some(v)) => Some(Snapshot::new(v).map_err(ServerError::from)?),
            (Op::Upsert, None) => {
                return Err(ServerError::Corrupt {
                    detail: format!(
                        "sync_changes.seq {} is an upsert with no snapshot",
                        self.seq
                    ),
                });
            }
            (Op::Delete, None) => None,
            (Op::Delete, Some(_)) => {
                return Err(ServerError::Corrupt {
                    detail: format!(
                        "sync_changes.seq {} is a delete carrying a snapshot",
                        self.seq
                    ),
                });
            }
        };

        let seq = u64::try_from(self.seq).map_err(|_| ServerError::Corrupt {
            detail: format!("sync_changes.seq {} is negative", self.seq),
        })?;
        let row_version = u64::try_from(self.row_version).map_err(|_| ServerError::Corrupt {
            detail: format!("sync_changes.row_version {} is negative", self.row_version),
        })?;
        let schema_version =
            u16::try_from(self.schema_version).map_err(|_| ServerError::Corrupt {
                detail: format!(
                    "sync_changes.schema_version {} does not fit the wire's range",
                    self.schema_version
                ),
            })?;

        Ok(Change {
            seq: Seq::new(seq)?,
            entity: EntityName::new(self.entity)?,
            entity_id: EntityId::new(self.entity_id)?,
            op,
            snapshot,
            row_version: RowVersion::new(row_version)?,
            schema_version: SchemaVersion::new(schema_version)?,
        })
    }
}

/// Fills one batch from changes already read in `seq` order.
///
/// `candidates` must be the changes for `scope` strictly after the client's cursor, in ascending
/// `seq`, and may be longer than one batch will hold — that is the point. Whatever does not fit
/// sets `has_more`.
///
/// # Errors
/// Returns [`ServerError::Corrupt`] if a row cannot be encoded, and propagates validation
/// failures from [`ChangeRow::into_change`].
pub fn fill_batch<C: Compressor>(
    scope: &ScopeId,
    cursor: Cursor,
    candidates: Vec<Change>,
    more_beyond: bool,
    budget_bytes: usize,
    compressor: &C,
    digest: HexString,
) -> Result<Batch, ServerError> {
    let available = candidates.len();
    let mut chosen: Vec<Change> = Vec::new();

    // The batch's canonical bytes, grown in place. Each change is encoded once and spliced in
    // before the closing bracket; a change that does not fit is rolled back by truncating.
    //
    // The obvious version — clone the chosen list and re-encode all of it per candidate — is
    // O(n^2) bytes of JSON serialisation, which is exactly the defect #53 removed from the
    // client's `build_push`. Writing it again here would have been the same bug on the other
    // side of the wire, where the snapshots are larger.
    let mut buf: Vec<u8> = vec![b'[', b']'];

    for change in candidates {
        let one = canonical::to_vec(&change).map_err(|_| ServerError::Corrupt {
            detail: "a change in the log could not be encoded".to_owned(),
        })?;

        let rollback = buf.len();
        buf.pop();
        if !chosen.is_empty() {
            buf.push(b',');
        }
        buf.extend_from_slice(&one);
        buf.push(b']');

        // One change always goes, even alone over budget. `docs/spec.md` §2: a single change
        // larger than the budget is delivered alone rather than stalling the cursor. Refusing
        // would wedge the scope — the client asks forever, the server has something it will not
        // send, and the cursor never moves again.
        if compressor.compressed_len(&buf) > budget_bytes && !chosen.is_empty() {
            buf.truncate(rollback - 1);
            buf.push(b']');
            break;
        }

        chosen.push(change);
    }

    let next_cursor = chosen
        .last()
        .map_or(cursor, |c| Cursor::new(c.seq.get()).unwrap_or(cursor));

    Ok(Batch {
        scope: scope.clone(),
        // `next_cursor` covers exactly what was sent, and no further. Advancing past undelivered
        // changes would skip them silently; leaving it short would re-deliver what was just
        // applied, which the client refuses — wedging the scope one round trip at a time.
        next_cursor,
        // True when anything was left behind — by the byte budget here, or by the row ceiling in
        // the query that produced these candidates. Both can truncate, and only the caller knows
        // about the second: a hundred small tombstones fit a 100 KB budget easily, so every
        // candidate would be chosen and `has_more` would read `false` while rows remain, leaving
        // the scope sitting still until something else happened to it.
        has_more: chosen.len() < available || more_beyond,
        checksum: batch_checksum(&chosen)?,
        digest,
        changes: chosen,
    })
}

/// The checksum carried with a batch.
///
/// Computed over the canonical encoding of the **changes**, not of the whole batch. `docs/spec.md`
/// §5 says "over its canonical encoding", which is self-referential while `checksum` is itself a
/// field of the batch — a checksum cannot cover the bytes it is part of. Covering the changes is
/// the reading that means something: those are the bytes corruption would damage, and the ones the
/// client re-fetches when it does.
fn batch_checksum(changes: &[Change]) -> Result<HexString, ServerError> {
    credsync_protocol::checksum(&changes).map_err(|_| ServerError::Corrupt {
        detail: "a batch could not be encoded for checksumming".to_owned(),
    })
}
