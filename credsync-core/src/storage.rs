//! What the engine asks to be written, and what it hears back.
//!
//! # One transaction, or none
//!
//! Every op in a batch commits together or not at all. This is not a convenience — it is the
//! rule that makes `docs/spec.md` §4 true: *"the cursor is persisted in the same storage
//! transaction as the rows it covers"*, so a process killed mid-apply resumes with cursor and
//! rows consistent.
//!
//! Split those writes across two transactions and a kill between them leaves a client whose
//! cursor claims changes it never applied. It will never fetch them again, and the rows are gone
//! silently, forever, with every subsequent sync reporting success. That is the exact failure
//! this engine exists to make impossible, and it is one refactor away at all times.
//!
//! # Rows are snapshots
//!
//! There is no `PatchRow`. `docs/spec.md` §1: snapshots, not diffs; deletes are tombstones.

use credsync_protocol::{
    Command, Cursor, EntityId, EntityName, HexString, RowVersion, SchemaVersion, ScopeId, Snapshot,
};

/// One write in a storage transaction.
///
/// Later slices extend this — the outbox state machine at CS-8 (#9), schema migrations at CS-20
/// (#21). Hence `#[non_exhaustive]`: adding a variant must not be a breaking change for adapters
/// outside this repository, and the conformance suite is what proves an adapter handles them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StorageOp {
    /// Insert or replace a row, whole.
    UpsertRow {
        /// Which entity the row belongs to.
        entity: EntityName,
        /// The row's identifier.
        entity_id: EntityId,
        /// The full row.
        snapshot: Snapshot,
        /// Server-assigned row version.
        row_version: RowVersion,
        /// The schema this snapshot was written under.
        schema_version: SchemaVersion,
    },
    /// Tombstone a row.
    ///
    /// Carries no snapshot, matching the wire (`docs/spec.md` §2.1), and no `row_version`: the
    /// row is gone, and a version on a corpse is a field that can only ever disagree with
    /// something.
    DeleteRow {
        /// Which entity the row belonged to.
        entity: EntityName,
        /// The row's identifier.
        entity_id: EntityId,
    },
    /// Move a scope's cursor.
    ///
    /// Belongs in the same batch as the rows it covers. See the module docs for what happens
    /// when it is not.
    SetCursor {
        /// The scope whose cursor moves.
        scope: ScopeId,
        /// Where it moves to.
        cursor: Cursor,
    },
    /// Record a scope's digest after applying a batch.
    ///
    /// Also in the same transaction as the rows, for the same reason: a digest that describes
    /// rows the database does not hold reports divergence that is not there.
    SetScopeDigest {
        /// The scope the digest describes.
        scope: ScopeId,
        /// The digest, 32 lowercase hex characters.
        digest: HexString,
    },
    /// Append a command to the outbox.
    ///
    /// `schema_version` is recorded with it because `docs/spec.md` §7 requires that an upgraded
    /// app migrate queued commands forward before pushing. A command that has sat in the outbox
    /// through an app update must still be sent under a schema the server accepts — and it is
    /// queued, never dropped.
    EnqueueCommand {
        /// The command as it will go on the wire.
        command: Command,
        /// The schema version it was authored under.
        schema_version: SchemaVersion,
    },
}

/// What a completed transaction reports back.
///
/// Deliberately thin. The engine knows what it asked for, so echoing the writes back would be
/// two descriptions of one fact — and the one from the database would quietly win.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxOutcome {
    /// How many ops were committed.
    ///
    /// Checked against the number submitted. An adapter that silently applies a subset is a
    /// worse problem than one that fails outright, because it looks like success.
    pub applied: usize,
}

impl TxOutcome {
    /// Reports a transaction that committed `applied` ops.
    #[must_use]
    pub const fn new(applied: usize) -> Self {
        Self { applied }
    }
}
