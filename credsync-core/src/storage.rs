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
    Command, CommandId, Cursor, EntityId, EntityName, HexString, Payload, RowVersion,
    SchemaVersion, ScopeId, Snapshot,
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
    /// Remove an outbox entry **and record why**, in one transaction.
    ///
    /// The only way an entry leaves the outbox. There is deliberately no `DeleteCommand` op:
    /// "never silently dropped" (`docs/spec.md` §3.3) is enforced by there being no operation
    /// that could drop one, rather than by every adapter remembering not to.
    ///
    /// Both halves must commit together. An entry removed without its outcome recorded is a
    /// write the user was told would be saved, gone with no trace and no explanation — which is
    /// exactly the failure the outbox exists to prevent.
    ResolveCommand {
        /// Which command was resolved.
        id: CommandId,
        /// What the host decided.
        resolution: crate::outbox::Resolution,
    },

    /// Preserve a client edit that lost, so the user can get it back.
    ///
    /// `docs/spec.md` §6: for owner drafts, *"the losing version returns to the device and is
    /// stored as a recovered draft. Silent loss is a protocol violation, not a tradeoff."*
    ///
    /// Written whenever a queued command is resolved as `superseded` — the server applied
    /// last-write-wins and this edit lost. The user's text is in that command's payload and
    /// exists nowhere else on the device once the entry leaves the outbox, so it is preserved
    /// here in the **same transaction** that resolves it. Two transactions, and a crash in
    /// between loses exactly the work this op exists to keep.
    SaveRecoveredDraft {
        /// Which entity the edit was for.
        entity: EntityName,
        /// The command whose payload this was, so the UI can tie it to what the user did.
        command_id: CommandId,
        /// The user's content, exactly as it was submitted.
        payload: Payload,
    },

    /// Record how many times a scope has diverged, so escalation survives a restart.
    ///
    /// **Durable on purpose.** The count exists to stop a scope being rebuilt forever, and the
    /// device that most needs stopping is the one crash-looping — which restarts between every
    /// attempt. An in-memory count would reset each time and the loop would run for the life of the
    /// install, re-downloading the same scope on a data budget the user is paying for.
    RecordDivergence {
        /// The scope that diverged.
        scope: ScopeId,
        /// How many times it has now diverged.
        attempts: u32,
        /// Whether a rebuild has since agreed.
        ///
        /// The count alone cannot tell "broken, rebuild it" from "was broken, rebuilt, fine now".
        /// Without this flag a restart reads a healed scope as tainted and clears and re-downloads
        /// it on every launch for the life of the install — the loop the count exists to prevent,
        /// moved from the escalation path to the heal path.
        healed: bool,
    },

    /// Drop every row of a scope, for a re-bootstrap after divergence. `docs/spec.md` §5.
    ///
    /// **Required, not an optimisation.** A fresh bootstrap (`after = 0`) carries no tombstones —
    /// a device starting from nothing has no row to delete — so rows this client holds that the
    /// server no longer has would survive the rebuild and keep the digest wrong forever. Clearing
    /// first is what makes the rebuild a rebuild rather than a merge.
    ///
    /// Scoped, because a tainted scope must not disturb the others (`docs/spec.md` §5). The
    /// outbox is **not** touched: those commands have not been sent yet, and losing them to fix a
    /// read-side problem would be the cure doing more damage than the disease.
    ClearScope {
        /// The scope whose cursor and digest are being reset.
        scope: ScopeId,
        /// The entities mapped to that scope, whose rows are to be dropped.
        ///
        /// Carried rather than looked up, because an adapter does not hold the registry and rows
        /// are keyed by `(entity, entity_id)` — `docs/spec.md` §1 maps each entity to exactly one
        /// scope, so this list is that mapping read backwards. An op that expected the adapter to
        /// know it would be an op every adapter could implement differently.
        entities: Vec<EntityName>,
    },

    /// Set a row aside because it could not be migrated, keeping the original bytes.
    ///
    /// `docs/spec.md` §7 requires the client to apply registered up-migrations. When one is
    /// missing or fails, the choice is between writing a document the app cannot read, discarding
    /// it, or setting it aside — and the first two are both data loss, one noisy and one silent.
    ///
    /// **The snapshot is stored exactly as it arrived.** A quarantine that stored a half-migrated
    /// value would destroy the only copy of what the server actually sent, so the row could never
    /// be recovered by a later app version that does know the migration. That later version is the
    /// entire point of keeping it.
    ///
    /// The row's `row_version` still contributes to the scope digest, because the device *has*
    /// received it — it simply cannot read it. Leaving it out would report divergence against a
    /// server the client has not actually diverged from, and send it re-bootstrapping into the
    /// same unreadable row.
    QuarantineRow {
        /// Which entity the row belongs to.
        entity: EntityName,
        /// The row's identifier.
        entity_id: EntityId,
        /// The row exactly as it arrived, unmigrated.
        snapshot: Snapshot,
        /// The version the snapshot is written under, not the one the app wanted.
        schema_version: SchemaVersion,
        /// Why it could not be migrated, for the operator and the user.
        reason: String,
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
