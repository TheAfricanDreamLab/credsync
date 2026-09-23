//! Test doubles for the four traits, and builders for the wire types the apply path consumes.
//!
//! [`FakeStorage`] is the interesting one. It is written to be **atomic in the same way a real
//! adapter must be**: when a transaction is set to fail, ops are applied to a scratch copy which
//! is then discarded, rather than simply returning an error before touching anything. A fake that
//! refused early would pass an atomicity test that a genuinely half-applying adapter would also
//! pass, which is no test at all.

#![allow(dead_code, unreachable_pub)]

use credsync_core::{Clock, Entropy, Transport, TransportError, WireRequest};
use credsync_core::{RequestId, Storage, StorageError, StorageOp, Timestamp, TxOutcome};
use credsync_protocol::{
    Batch, Change, Cursor, EntityId, EntityName, HexString, Op, RowVersion, SchemaVersion,
    ScopeDigest, ScopeId, Seq, Snapshot,
};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

/// A row as stored locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    pub snapshot: Snapshot,
    pub row_version: RowVersion,
    pub schema_version: SchemaVersion,
}

/// Key identifying a row. `(entity, entity_id)` is unique because the registry maps each entity
/// to one scope (`docs/spec.md` §1).
pub type RowKey = (EntityName, EntityId);

/// An in-memory storage adapter that commits whole or not at all.
#[derive(Debug, Default)]
pub struct FakeStorage {
    rows: BTreeMap<RowKey, StoredRow>,
    cursors: BTreeMap<ScopeId, Cursor>,
    digests: BTreeMap<ScopeId, HexString>,
    /// Transactions attempted, successful or not.
    pub attempts: usize,
    /// Transactions that committed.
    pub commits: usize,
    /// When set, the next transaction stages every op and then discards the lot.
    pub fail_next: Option<StorageError>,
    /// When set, the next read fails.
    pub fail_read: Option<StorageError>,
}

impl FakeStorage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every live row, in a deterministic order.
    #[must_use]
    pub fn live_rows(&self) -> Vec<(EntityName, EntityId, RowVersion)> {
        self.rows
            .iter()
            .map(|((e, i), r)| (e.clone(), i.clone(), r.row_version))
            .collect()
    }

    /// The digest recomputed from scratch over the live rows.
    ///
    /// This is the independent check the property test compares against: it never consults the
    /// engine's running digest, so agreement between them means something.
    #[must_use]
    pub fn digest_from_scratch(&self) -> ScopeDigest {
        let rows = self.live_rows();
        ScopeDigest::from_rows(rows.iter().map(|(e, i, v)| (e, i, *v)))
    }

    #[must_use]
    pub fn row(&self, entity: &EntityName, entity_id: &EntityId) -> Option<&StoredRow> {
        self.rows.get(&(entity.clone(), entity_id.clone()))
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn cursor(&self, scope: &ScopeId) -> Option<Cursor> {
        self.cursors.get(scope).copied()
    }

    #[must_use]
    pub fn digest(&self, scope: &ScopeId) -> Option<&HexString> {
        self.digests.get(scope)
    }

    /// Applies ops to a snapshot of state, returning the new state.
    fn staged(
        &self,
        ops: &[StorageOp],
    ) -> (
        BTreeMap<RowKey, StoredRow>,
        BTreeMap<ScopeId, Cursor>,
        BTreeMap<ScopeId, HexString>,
    ) {
        let mut rows = self.rows.clone();
        let mut cursors = self.cursors.clone();
        let mut digests = self.digests.clone();

        for op in ops {
            match op {
                StorageOp::UpsertRow {
                    entity,
                    entity_id,
                    snapshot,
                    row_version,
                    schema_version,
                } => {
                    rows.insert(
                        (entity.clone(), entity_id.clone()),
                        StoredRow {
                            snapshot: snapshot.clone(),
                            row_version: *row_version,
                            schema_version: *schema_version,
                        },
                    );
                }
                StorageOp::DeleteRow { entity, entity_id } => {
                    rows.remove(&(entity.clone(), entity_id.clone()));
                }
                StorageOp::SetCursor { scope, cursor } => {
                    cursors.insert(scope.clone(), *cursor);
                }
                StorageOp::SetScopeDigest { scope, digest } => {
                    digests.insert(scope.clone(), digest.clone());
                }
                StorageOp::EnqueueCommand { .. } => {}
                _ => {}
            }
        }

        (rows, cursors, digests)
    }
}

impl Storage for FakeStorage {
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError> {
        self.attempts += 1;

        // Staged first, exactly as a real transaction would be, so a rollback discards work that
        // really was performed rather than work that was never attempted.
        let (rows, cursors, digests) = self.staged(ops);

        if let Some(e) = self.fail_next.take() {
            // Rolled back: the staged state is dropped here, untouched by the fields below.
            return Err(e);
        }

        self.rows = rows;
        self.cursors = cursors;
        self.digests = digests;
        self.commits += 1;
        Ok(TxOutcome::new(ops.len()))
    }

    fn row_version(
        &self,
        entity: &EntityName,
        entity_id: &EntityId,
    ) -> Result<Option<RowVersion>, StorageError> {
        if let Some(e) = &self.fail_read {
            return Err(e.clone());
        }
        Ok(self
            .rows
            .get(&(entity.clone(), entity_id.clone()))
            .map(|r| r.row_version))
    }
}

/// A handle onto a [`FakeStorage`] that the engine can own while the test still watches it.
///
/// The engine takes its `Storage` by value (D-037), so without this a test could never inspect
/// what was written. `Rc<RefCell<_>>` rather than a public accessor on `Engine`: the engine's API
/// should not grow a hole purely so tests can peer through it — and being `!Send`, this doubles
/// as ongoing proof that the single-threaded property from CS-6 still holds.
#[derive(Debug, Clone, Default)]
pub struct SharedStorage(pub Rc<RefCell<FakeStorage>>);

impl SharedStorage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads something out of the underlying store.
    pub fn with<R>(&self, f: impl FnOnce(&FakeStorage) -> R) -> R {
        f(&self.0.borrow())
    }

    /// Mutates the underlying store — used to arm a failure before a call.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut FakeStorage) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

impl Storage for SharedStorage {
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError> {
        self.0.borrow_mut().transact(ops)
    }

    fn row_version(
        &self,
        entity: &EntityName,
        entity_id: &EntityId,
    ) -> Result<Option<RowVersion>, StorageError> {
        self.0.borrow().row_version(entity, entity_id)
    }
}

/// A clock the test advances by hand.
#[derive(Debug, Default)]
pub struct FakeClock(pub Rc<Cell<i64>>);

impl Clock for FakeClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0.get())
    }
}

/// Counter-driven entropy: deterministic, which is the whole requirement.
#[derive(Debug, Default)]
pub struct FakeEntropy(pub Rc<Cell<u64>>);

impl Entropy for FakeEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        for slot in buf.iter_mut() {
            let next = self.0.get().wrapping_add(1);
            self.0.set(next);
            *slot = (next & 0xff) as u8;
        }
    }
}

/// A transport that records what it was handed and never answers.
#[derive(Debug, Default)]
pub struct FakeTransport {
    pub sent: Rc<RefCell<Vec<WireRequest>>>,
    next_id: Cell<u64>,
}

impl Transport for FakeTransport {
    fn enqueue(&mut self, req: WireRequest) -> Result<RequestId, TransportError> {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        self.sent.borrow_mut().push(req);
        Ok(RequestId::new(id))
    }
}

// ---------------------------------------------------------------------------------------------
// Builders. Short names because the tests read better for it.
// ---------------------------------------------------------------------------------------------

#[must_use]
pub fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

#[must_use]
pub fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

#[must_use]
pub fn id(s: &str) -> EntityId {
    EntityId::new(s).expect("valid entity_id")
}

#[must_use]
pub fn snapshot(body: &str) -> Snapshot {
    Snapshot::new(json!({ "body": body })).expect("valid snapshot")
}

#[must_use]
pub fn hex(s: &str) -> HexString {
    HexString::new(s).expect("valid hex")
}

#[must_use]
pub fn schema() -> SchemaVersion {
    SchemaVersion::new(1).expect("valid schema_version")
}

/// An upsert of `entity_id` at `seq`, carrying `row_version`.
#[must_use]
pub fn upsert(seq: u64, entity_id: &str, row_version: u64) -> Change {
    Change {
        seq: Seq::new(seq).expect("valid seq"),
        entity: entity(),
        entity_id: id(entity_id),
        op: Op::Upsert,
        snapshot: Some(snapshot(entity_id)),
        row_version: RowVersion::new(row_version).expect("valid row_version"),
        schema_version: schema(),
    }
}

/// A tombstone for `entity_id` at `seq`.
#[must_use]
pub fn tombstone(seq: u64, entity_id: &str, row_version: u64) -> Change {
    Change {
        seq: Seq::new(seq).expect("valid seq"),
        entity: entity(),
        entity_id: id(entity_id),
        op: Op::Delete,
        snapshot: None,
        row_version: RowVersion::new(row_version).expect("valid row_version"),
        schema_version: schema(),
    }
}

/// A batch carrying `changes`, with `next_cursor` defaulting to the highest `seq`.
///
/// The digest is a placeholder; tests that care about divergence set it explicitly with
/// [`batch_with_digest`].
#[must_use]
pub fn batch(changes: Vec<Change>) -> Batch {
    let next = changes.iter().map(|c| c.seq.get()).max().unwrap_or(0);
    Batch {
        scope: scope(),
        changes,
        next_cursor: Cursor::new(next).expect("valid cursor"),
        has_more: false,
        checksum: hex("3f1a6c9d0e2b48571c83af4d5e60729b"),
        digest: hex("00000000000000000000000000000000"),
    }
}

/// A batch whose `digest` is whatever the caller says the server reported.
#[must_use]
pub fn batch_with_digest(changes: Vec<Change>, digest: HexString) -> Batch {
    Batch {
        digest,
        ..batch(changes)
    }
}

/// The engine used by every test here.
pub type TestEngine = credsync_core::Engine<FakeClock, FakeEntropy, SharedStorage, FakeTransport>;

/// A fresh engine and the handle onto the storage it owns.
#[must_use]
pub fn new_engine() -> (TestEngine, SharedStorage) {
    let storage = SharedStorage::new();
    let engine = credsync_core::Engine::new(
        FakeClock::default(),
        FakeEntropy::default(),
        storage.clone(),
        FakeTransport::default(),
    );
    (engine, storage)
}
