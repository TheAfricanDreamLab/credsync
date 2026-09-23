//! Test doubles for the four traits, and builders for the wire types the apply path consumes.
//!
//! [`FakeStorage`] is the interesting one. It is written to be **atomic in the same way a real
//! adapter must be**: when a transaction is set to fail, ops are applied to a scratch copy which
//! is then discarded, rather than simply returning an error before touching anything. A fake that
//! refused early would pass an atomicity test that a genuinely half-applying adapter would also
//! pass, which is no test at all.

#![allow(dead_code, unreachable_pub)]

use credsync_core::{
    Clock, Compressor, Entropy, OutboxEntry, Transport, TransportError, WireRequest,
};
use credsync_core::{RequestId, Storage, StorageError, StorageOp, Timestamp, TxOutcome};
use credsync_protocol::{
    Batch, Change, Command, CommandId, CommandName, CommandResult, ConflictClass, Cursor, EntityId,
    EntityName, EntityRegistration, HexString, Op, Payload, ProtocolVersion, PushResponse, Reason,
    RowVersion, SchemaVersion, ScopeDigest, ScopeId, Seq, Snapshot, Status,
};
use proptest::prelude::ProptestConfig;
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
    /// Outbox entries, in the order they were enqueued.
    pub queued: Vec<(CommandId, SchemaVersion)>,
    /// Recorded outcomes, in resolution order.
    pub resolved: Vec<(CommandId, credsync_core::Resolution)>,
    /// Recovered drafts: the command whose edit lost, its entity, and the content preserved.
    pub recovered: Vec<(CommandId, EntityName, Payload)>,
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
    ///
    /// Everything a transaction touches is staged here — rows, cursors, digests, and the outbox —
    /// so a rollback discards all of it together. Leaving the outbox out would make the atomicity
    /// tests pass while the very writes the outbox exists to protect were committed early.
    fn staged(&self, ops: &[StorageOp]) -> Snapshotted {
        let mut s = Snapshotted {
            rows: self.rows.clone(),
            cursors: self.cursors.clone(),
            digests: self.digests.clone(),
            queued: self.queued.clone(),
            resolved: self.resolved.clone(),
            recovered: self.recovered.clone(),
        };

        for op in ops {
            match op {
                StorageOp::UpsertRow {
                    entity,
                    entity_id,
                    snapshot,
                    row_version,
                    schema_version,
                } => {
                    s.rows.insert(
                        (entity.clone(), entity_id.clone()),
                        StoredRow {
                            snapshot: snapshot.clone(),
                            row_version: *row_version,
                            schema_version: *schema_version,
                        },
                    );
                }
                StorageOp::DeleteRow { entity, entity_id } => {
                    s.rows.remove(&(entity.clone(), entity_id.clone()));
                }
                StorageOp::SetCursor { scope, cursor } => {
                    s.cursors.insert(scope.clone(), *cursor);
                }
                StorageOp::SetScopeDigest { scope, digest } => {
                    s.digests.insert(scope.clone(), digest.clone());
                }
                StorageOp::EnqueueCommand {
                    command,
                    schema_version,
                } => {
                    s.queued.push((command.id, *schema_version));
                }
                StorageOp::ResolveCommand { id, resolution } => {
                    s.queued.retain(|(qid, _)| qid != id);
                    s.resolved.push((*id, resolution.clone()));
                }
                StorageOp::SaveRecoveredDraft {
                    entity,
                    command_id,
                    payload,
                } => {
                    s.recovered
                        .push((*command_id, entity.clone(), payload.clone()));
                }
                _ => {}
            }
        }

        s
    }
}

/// A full snapshot of everything a transaction can touch.
#[derive(Debug, Clone)]
pub struct Snapshotted {
    rows: BTreeMap<RowKey, StoredRow>,
    cursors: BTreeMap<ScopeId, Cursor>,
    digests: BTreeMap<ScopeId, HexString>,
    queued: Vec<(CommandId, SchemaVersion)>,
    resolved: Vec<(CommandId, credsync_core::Resolution)>,
    recovered: Vec<(CommandId, EntityName, Payload)>,
}

impl Storage for FakeStorage {
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError> {
        self.attempts += 1;

        // Staged first, exactly as a real transaction would be, so a rollback discards work that
        // really was performed rather than work that was never attempted.
        let staged = self.staged(ops);

        if let Some(e) = self.fail_next.take() {
            // Rolled back: the staged state is dropped here, untouched by the fields below.
            return Err(e);
        }

        self.rows = staged.rows;
        self.cursors = staged.cursors;
        self.digests = staged.digests;
        self.queued = staged.queued;
        self.resolved = staged.resolved;
        self.recovered = staged.recovered;
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

/// A compressor that divides, so a test can ask for a given compression ratio.
///
/// `divisor` of 1 means incompressible; 10 means the payload shrinks tenfold. Being able to set
/// the ratio is what lets a test prove the budget is applied to **compressed** bytes: the same
/// commands, the same budget, a different ratio, and a different number of entries fit. A fake
/// that always returned the input length could not distinguish that from a row count.
#[derive(Debug, Clone)]
pub struct FakeCompressor {
    pub divisor: usize,
    pub calls: Rc<Cell<usize>>,
    /// The last buffer this compressor was asked to measure.
    ///
    /// `build_push` hands it the real candidate bytes, so recording them is how a test checks
    /// that the incrementally spliced buffer matches a whole-list encode (#53).
    pub last_seen: Rc<RefCell<Vec<u8>>>,
}

impl FakeCompressor {
    #[must_use]
    pub fn with_ratio(divisor: usize) -> Self {
        Self {
            divisor: divisor.max(1),
            calls: Rc::new(Cell::new(0)),
            last_seen: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl Default for FakeCompressor {
    fn default() -> Self {
        Self::with_ratio(1)
    }
}

impl Compressor for FakeCompressor {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        self.calls.set(self.calls.get() + 1);
        self.last_seen.borrow_mut().clear();
        self.last_seen.borrow_mut().extend_from_slice(bytes);
        bytes.len().div_ceil(self.divisor)
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

/// An upsert against a named entity, for conflict-class tests.
#[must_use]
pub fn upsert_of(entity: &str, seq: u64, entity_id: &str, row_version: u64) -> Change {
    Change {
        entity: EntityName::new(entity).expect("valid entity"),
        ..upsert(seq, entity_id, row_version)
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
pub type TestEngine =
    credsync_core::Engine<FakeClock, FakeEntropy, SharedStorage, FakeTransport, FakeCompressor>;

/// A fresh engine and the handle onto the storage it owns.
#[must_use]
pub fn new_engine() -> (TestEngine, SharedStorage) {
    new_engine_with(FakeCompressor::with_ratio(1))
}

/// A fresh engine whose compressor reports a chosen ratio.
///
/// Comes with the default registry from [`register_defaults`], because an engine with an empty
/// registry accepts no commands at all — which is correct, and would make every outbox test a
/// test of the registry instead.
#[must_use]
pub fn new_engine_with(compressor: FakeCompressor) -> (TestEngine, SharedStorage) {
    let storage = SharedStorage::new();
    let mut engine = credsync_core::Engine::new(
        FakeClock::default(),
        FakeEntropy::default(),
        storage.clone(),
        FakeTransport::default(),
        compressor,
    );
    register_defaults(engine.registry_mut());
    (engine, storage)
}

/// An engine with a deliberately empty registry.
#[must_use]
pub fn new_engine_unregistered() -> (TestEngine, SharedStorage) {
    let storage = SharedStorage::new();
    let engine = credsync_core::Engine::new(
        FakeClock::default(),
        FakeEntropy::default(),
        storage.clone(),
        FakeTransport::default(),
        FakeCompressor::with_ratio(1),
    );
    (engine, storage)
}

/// One entity per conflict class, plus a command targeting each.
///
/// The names mirror `docs/spec.md` §6's own examples: grades are institution truth, reflections
/// are owner drafts, submissions are an append-only stream.
pub fn register_defaults(registry: &mut credsync_core::Registry) {
    for (entity, class) in [
        ("reflections", ConflictClass::OwnerDraft),
        ("grades", ConflictClass::ServerAuthoritative),
        ("submissions", ConflictClass::AppendOnly),
    ] {
        registry.register_entity(EntityRegistration {
            entity: EntityName::new(entity).expect("valid entity"),
            scope: scope(),
            conflict_class: class,
            schema_version: schema(),
        });
    }
    for (command, entity) in [
        ("submit_reflection", "reflections"),
        ("amend_grade", "grades"),
        ("add_submission", "submissions"),
    ] {
        registry.register_command(
            CommandName::new(command).expect("valid name"),
            EntityName::new(entity).expect("valid entity"),
        );
    }
}

/// A command with an explicit name, for registry tests.
#[must_use]
pub fn named_command(n: u8, name: &str) -> Command {
    Command {
        name: CommandName::new(name).expect("valid name"),
        ..command(n, 8)
    }
}

// ---------------------------------------------------------------------------------------------
// Command builders
// ---------------------------------------------------------------------------------------------

/// A UUIDv7 with `n` folded into its tail, so tests can name commands readably.
#[must_use]
pub fn command_id(n: u8) -> CommandId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x91;
    bytes[6] = 0x70;
    bytes[15] = n;
    CommandId::from_bytes(bytes).expect("version nibble is 7")
}

/// A command whose payload is `filler_len` bytes of repeated text.
///
/// The filler is a single repeated character so it compresses predictably — which is what lets a
/// test reason about compressed size rather than merely measure it.
#[must_use]
pub fn command(n: u8, filler_len: usize) -> Command {
    Command {
        id: command_id(n),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope(),
        payload: Payload::new(serde_json::json!({ "body": "x".repeat(filler_len) }))
            .expect("valid payload"),
        client_ts: 1_756_137_600_000,
        checksum: hex("7b2e15c0a94d3f68e0517cab2d94f831"),
    }
}

/// An outbox entry wrapping [`command`].
#[must_use]
pub fn entry(n: u8, filler_len: usize) -> OutboxEntry {
    OutboxEntry::new(command(n, filler_len), schema())
}

#[must_use]
pub fn protocol() -> ProtocolVersion {
    ProtocolVersion::new(1).expect("valid protocol")
}

/// A push response answering `results`.
#[must_use]
pub fn push_response(results: Vec<CommandResult>) -> PushResponse {
    PushResponse {
        protocol: protocol(),
        results,
    }
}

#[must_use]
pub fn applied(n: u8, server_seq: u64) -> CommandResult {
    CommandResult {
        id: command_id(n),
        status: Status::Applied,
        reason: None,
        server_seq: Some(Seq::new(server_seq).expect("valid seq")),
    }
}

#[must_use]
pub fn rejected(n: u8, why: &str) -> CommandResult {
    CommandResult {
        id: command_id(n),
        status: Status::Rejected,
        reason: Some(Reason::new(why).expect("valid reason")),
        server_seq: None,
    }
}

#[must_use]
pub fn superseded(n: u8) -> CommandResult {
    CommandResult {
        id: command_id(n),
        status: Status::Superseded,
        reason: None,
        server_seq: None,
    }
}

/// A `proptest` configuration whose case count can be turned up or down from the environment.
///
/// `default_cases` is what this suite runs when nothing says otherwise. `PROPTEST_CASES`
/// overrides it, which is what lets one test suite serve three very different jobs (CS-10):
///
/// - **PR CI** runs the default, which is the count these tests have always run at.
/// - **Nightly** turns it far up, because depth is worth an hour when nobody is waiting.
/// - **Miri** turns it far down. Miri interprets rather than executes and is roughly two orders
///   of magnitude slower, so it multiplies against the case count brutally — the default never
///   finished in over an hour. The Miri job checks for undefined behaviour, not input coverage,
///   and a handful of cases does that job.
///
/// Reading the environment here rather than hardcoding is the whole mechanism. `proptest`'s own
/// `ProptestConfig::default()` already honours `PROPTEST_CASES`, but an explicit `cases: 256`
/// silently overrides it — which is exactly what these suites used to do, and why setting the
/// variable changed nothing at all.
#[must_use]
pub fn config(default_cases: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default_cases);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}
