//! The five traits, implemented against simulated reality.
//!
//! These are the seam the whole design was built around. The engine cannot tell the difference
//! between these and the real thing, which is what lets the **same core bytes** run in production
//! and under a simulator that skews its clock by three days and kills it mid-write.
//!
//! Everything here is shared through `Rc<RefCell<_>>`, because the engine takes each trait by
//! value while the world still needs to reach in — advance the clock, fail the next transaction,
//! read what a device stored. Being `!Send` is a feature rather than a cost: it is ongoing proof
//! that the single-threaded property from CS-6 still holds.

use crate::fault::SKEW_MAGNITUDE_MS;
use credsync_core::{
    Clock, Compressor, Entropy, RequestId, Storage, StorageError, StorageOp, Timestamp, Transport,
    TransportError, TxOutcome, WireRequest,
};
use credsync_protocol::{
    Command, CommandId, Cursor, EntityId, EntityName, HexString, Payload, RowVersion,
    SchemaVersion, ScopeId, Snapshot,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

// ---------------------------------------------------------------------------------------------
// Clock
// ---------------------------------------------------------------------------------------------

/// Simulated time, plus whatever this device's clock believes about it.
///
/// `docs/spec.md` §6 says device clocks lie, so the simulator makes them lie: each device carries
/// a fixed skew of up to ±3 days and a drift that accumulates as simulated time passes. The world
/// knows the true time; the device does not, and must never need to.
#[derive(Debug)]
pub struct ClockState {
    /// True simulated time, owned by the world.
    pub now_ms: i64,
    /// This device's constant offset from it.
    pub skew_ms: i64,
    /// Parts-per-million this device's clock runs fast or slow.
    pub drift_ppm: i64,
    /// True time when the device last started, so drift accumulates from there.
    pub epoch_ms: i64,
}

impl ClockState {
    /// What this device thinks the time is.
    #[must_use]
    pub const fn apparent_ms(&self) -> i64 {
        let elapsed = self.now_ms - self.epoch_ms;
        let drift = elapsed.saturating_mul(self.drift_ppm) / 1_000_000;
        self.now_ms
            .saturating_add(self.skew_ms)
            .saturating_add(drift)
    }
}

/// A [`Clock`] reading simulated time through one device's skew.
#[derive(Debug, Clone)]
pub struct SimClock(pub Rc<RefCell<ClockState>>);

impl SimClock {
    /// A clock skewed by `skew_ms` and drifting at `drift_ppm`.
    #[must_use]
    pub fn new(now_ms: i64, skew_ms: i64, drift_ppm: i64) -> Self {
        debug_assert!(
            skew_ms.abs() <= SKEW_MAGNITUDE_MS,
            "skew beyond the menu's ±3 days"
        );
        Self(Rc::new(RefCell::new(ClockState {
            now_ms,
            skew_ms,
            drift_ppm,
            epoch_ms: now_ms,
        })))
    }

    /// Moves true time forward.
    pub fn advance_to(&self, now_ms: i64) {
        self.0.borrow_mut().now_ms = now_ms;
    }

    /// True simulated time, which only the world may consult.
    #[must_use]
    pub fn true_now_ms(&self) -> i64 {
        self.0.borrow().now_ms
    }
}

impl Clock for SimClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0.borrow().apparent_ms())
    }
}

// ---------------------------------------------------------------------------------------------
// Entropy
// ---------------------------------------------------------------------------------------------

/// Entropy drawn from the run's seed.
///
/// Every byte of randomness in the system arrives through here, which is what `docs/spec.md` §2
/// requires for backoff jitter to replay identically.
#[derive(Debug, Clone)]
pub struct SimEntropy(pub Rc<RefCell<crate::rng::Rng>>);

impl SimEntropy {
    /// An entropy source from a seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(Rc::new(RefCell::new(crate::rng::Rng::new(seed))))
    }
}

impl Entropy for SimEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        let mut rng = self.0.borrow_mut();
        for slot in buf.iter_mut() {
            *slot = (rng.next_u32() & 0xff) as u8;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------------------------

/// A row as a device holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRow {
    /// The row's content.
    pub snapshot: Snapshot,
    /// Server-assigned version.
    pub row_version: RowVersion,
    /// The schema it was written under.
    pub schema_version: SchemaVersion,
}

/// What the next transaction should do instead of succeeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageVerdict {
    /// Commit normally.
    #[default]
    Commit,
    /// Fail without committing. Nothing is written.
    FailBeforeCommit,
    /// Commit, then report failure — the process died between the write landing and the engine
    /// hearing about it. The engine must not advance its memory, and a restart must find the work
    /// already done.
    CommitThenLoseAck,
}

/// One device's local database.
#[derive(Debug, Default)]
pub struct Db {
    /// Live rows, keyed by `(entity, entity_id)`.
    pub rows: BTreeMap<(EntityName, EntityId), StoredRow>,
    /// Per-scope cursors.
    pub cursors: BTreeMap<ScopeId, Cursor>,
    /// Per-scope digests, as the device last computed them.
    pub digests: BTreeMap<ScopeId, HexString>,
    /// Queued commands, oldest first.
    pub outbox: Vec<(Command, SchemaVersion)>,
    /// Resolved commands and what happened to them.
    pub resolved: Vec<(CommandId, credsync_core::Resolution)>,
    /// Edits that lost a conflict and were preserved.
    pub recovered: Vec<(CommandId, EntityName, Payload)>,
    /// What the next transaction should do.
    pub verdict: StorageVerdict,
    /// Transactions that committed, for the trace.
    pub commits: u64,
    /// Rows set aside because they could not be migrated, with the bytes exactly as they arrived.
    ///
    /// `docs/spec.md` §7: a row whose schema this app cannot reach is quarantined rather than
    /// written unreadable or discarded. The original snapshot is kept so a later app version that
    /// knows the migration can recover it — which is the only reason to keep it.
    pub quarantine: Vec<(EntityId, Snapshot, SchemaVersion, String)>,
    /// Every command id this device has ever enqueued.
    ///
    /// The outbox drains and `resolved` only grows for commands that got an answer, so neither
    /// alone can tell you a command *vanished*. This is the set to check against: a command that
    /// is in here and in neither of the others left without a trace, and the user's write is gone.
    pub enqueued: std::collections::BTreeSet<CommandId>,
}

impl Db {
    /// Every command id this device has ever enqueued.
    #[must_use]
    pub fn enqueued_ids(&self) -> Vec<CommandId> {
        self.enqueued.iter().copied().collect()
    }

    /// A copy of this database, for tests that need to compare an intact state against a broken
    /// one built from it.
    ///
    /// Not a `Clone` impl: a database is a device's whole durable state, and making it casually
    /// cloneable invites a test that mutates the copy and asserts against the original.
    #[must_use]
    pub fn clone_for_test(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            cursors: self.cursors.clone(),
            digests: self.digests.clone(),
            outbox: self.outbox.clone(),
            resolved: self.resolved.clone(),
            recovered: self.recovered.clone(),
            verdict: self.verdict,
            commits: self.commits,
            enqueued: self.enqueued.clone(),
            quarantine: self.quarantine.clone(),
        }
    }

    /// Applies ops to a copy, so a rollback discards work that was really performed.
    fn staged(&self, ops: &[StorageOp]) -> Self {
        let mut next = Self {
            rows: self.rows.clone(),
            cursors: self.cursors.clone(),
            digests: self.digests.clone(),
            outbox: self.outbox.clone(),
            resolved: self.resolved.clone(),
            recovered: self.recovered.clone(),
            verdict: StorageVerdict::Commit,
            commits: self.commits,
            enqueued: self.enqueued.clone(),
            quarantine: self.quarantine.clone(),
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
                    next.rows.insert(
                        (entity.clone(), entity_id.clone()),
                        StoredRow {
                            snapshot: snapshot.clone(),
                            row_version: *row_version,
                            schema_version: *schema_version,
                        },
                    );
                }
                StorageOp::DeleteRow { entity, entity_id } => {
                    next.rows.remove(&(entity.clone(), entity_id.clone()));
                }
                StorageOp::SetCursor { scope, cursor } => {
                    next.cursors.insert(scope.clone(), *cursor);
                }
                StorageOp::SetScopeDigest { scope, digest } => {
                    next.digests.insert(scope.clone(), digest.clone());
                }
                StorageOp::EnqueueCommand {
                    command,
                    schema_version,
                } => {
                    next.outbox.push((command.clone(), *schema_version));
                    // Staged with the write, so an enqueue that rolls back is not remembered as
                    // having happened. A test asserting "this command vanished" must not fire for
                    // a command the transaction never committed in the first place.
                    next.enqueued.insert(command.id);
                }
                StorageOp::ResolveCommand { id, resolution } => {
                    next.outbox.retain(|(c, _)| c.id != *id);
                    next.resolved.push((*id, resolution.clone()));
                }
                StorageOp::QuarantineRow {
                    entity_id,
                    snapshot,
                    schema_version,
                    reason,
                    ..
                } => {
                    next.quarantine.push((
                        entity_id.clone(),
                        snapshot.clone(),
                        *schema_version,
                        reason.clone(),
                    ));
                }
                StorageOp::SaveRecoveredDraft {
                    entity,
                    command_id,
                    payload,
                } => {
                    next.recovered
                        .push((*command_id, entity.clone(), payload.clone()));
                }
                _ => {}
            }
        }

        next
    }
}

/// A [`Storage`] over one device's [`Db`], with fault injection.
#[derive(Debug, Clone)]
pub struct SimStorage(pub Rc<RefCell<Db>>);

impl SimStorage {
    /// An empty database.
    #[must_use]
    pub fn new() -> Self {
        Self(Rc::new(RefCell::new(Db::default())))
    }
}

impl Default for SimStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for SimStorage {
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError> {
        let mut db = self.0.borrow_mut();
        let verdict = db.verdict;
        db.verdict = StorageVerdict::Commit;

        // Staged first, exactly as a real transaction would be. A fake that refused before
        // touching anything would pass an atomicity test that a genuinely half-applying adapter
        // would also pass, which is no test at all.
        let staged = db.staged(ops);

        match verdict {
            StorageVerdict::FailBeforeCommit => Err(StorageError::Transient {
                detail: "simulated failure before commit".to_owned(),
            }),
            StorageVerdict::Commit | StorageVerdict::CommitThenLoseAck => {
                let commits = staged.commits + 1;
                *db = staged;
                db.commits = commits;

                if matches!(verdict, StorageVerdict::CommitThenLoseAck) {
                    // Committed, but the engine is told it failed. This is the kill-between-
                    // transaction-and-ack case: the write is on disk and the engine does not know
                    // it. Recovery must come from replaying against a server that dedupes, not
                    // from the engine guessing.
                    return Err(StorageError::Transient {
                        detail: "simulated kill between commit and acknowledgement".to_owned(),
                    });
                }
                Ok(TxOutcome::new(ops.len()))
            }
        }
    }

    fn row_version(
        &self,
        entity: &EntityName,
        entity_id: &EntityId,
    ) -> Result<Option<RowVersion>, StorageError> {
        Ok(self
            .0
            .borrow()
            .rows
            .get(&(entity.clone(), entity_id.clone()))
            .map(|r| r.row_version))
    }
}

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/// A request handed to the network, waiting for the world to carry it.
#[derive(Debug, Clone)]
pub struct Outbound {
    /// The handle the engine will see quoted back.
    pub id: RequestId,
    /// What was sent.
    pub request: WireRequest,
}

/// A [`Transport`] that parks requests for the world to deliver.
///
/// Fire-and-forget by design: `enqueue` returns immediately and the answer arrives later, or
/// never. On the links credSync is built for, never is the median outcome.
#[derive(Debug, Clone)]
pub struct SimTransport {
    /// Requests waiting to be carried.
    pub pending: Rc<RefCell<Vec<Outbound>>>,
    next_id: Rc<RefCell<u64>>,
    /// While offline, `enqueue` refuses outright — a flap the device can see.
    pub online: Rc<RefCell<bool>>,
}

impl SimTransport {
    /// A transport with nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Rc::new(RefCell::new(Vec::new())),
            next_id: Rc::new(RefCell::new(0)),
            online: Rc::new(RefCell::new(true)),
        }
    }

    /// Takes everything queued since the last call.
    #[must_use]
    pub fn drain(&self) -> Vec<Outbound> {
        std::mem::take(&mut *self.pending.borrow_mut())
    }
}

impl Default for SimTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for SimTransport {
    fn enqueue(&mut self, req: WireRequest) -> Result<RequestId, TransportError> {
        if !*self.online.borrow() {
            return Err(TransportError::Unreachable {
                detail: "simulated connectivity flap".to_owned(),
            });
        }
        let mut next = self.next_id.borrow_mut();
        *next += 1;
        let id = RequestId::new(*next);
        self.pending.borrow_mut().push(Outbound {
            id,
            request: req.clone(),
        });
        Ok(id)
    }
}

// ---------------------------------------------------------------------------------------------
// Compressor
// ---------------------------------------------------------------------------------------------

/// A [`Compressor`] with a fixed, deterministic ratio.
///
/// Pure and stateless, which the trait requires: a compressor with adaptive state would make
/// batch composition differ between two runs of one seed, and determinism would be gone.
#[derive(Debug, Clone, Copy)]
pub struct SimCompressor {
    /// How much the payload shrinks. 1 means incompressible.
    pub divisor: usize,
}

impl SimCompressor {
    /// A compressor with the given ratio.
    #[must_use]
    pub const fn with_ratio(divisor: usize) -> Self {
        Self {
            divisor: if divisor == 0 { 1 } else { divisor },
        }
    }
}

impl Default for SimCompressor {
    fn default() -> Self {
        // Roughly what compact JSON of repetitive host documents achieves over gzip.
        Self::with_ratio(4)
    }
}

impl Compressor for SimCompressor {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        bytes.len().div_ceil(self.divisor)
    }
}
