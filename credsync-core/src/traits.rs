//! The four traits, and nothing else.
//!
//! Design v2.1 §4.1: *"Four traits parameterize everything that touches reality."* They are
//! **parameters of the core, never dependencies of it** — declared here, implemented elsewhere.
//! Production supplies a system clock, the platform's HTTP stack and SQLite; `credsync-sim`
//! supplies seeded fakes. The same core bytes run in both worlds, and that is the entire trick
//! that makes deterministic simulation possible.
//!
//! No implementation of any of these will ever live in this crate. If one appears, the crate has
//! stopped being sans-IO and the simulator has quietly stopped being able to find bugs.
//!
//! # Why none of these are `async`
//!
//! The core is synchronous. An `async fn` in a trait here would drag an executor into the
//! dependency graph and make the effect stream depend on task scheduling — which is exactly the
//! nondeterminism that deterministic replay cannot survive. Concurrency belongs in `credsyncd`
//! and in the bindings, outside this boundary.
//!
//! # Why none of these require `Send` or `Sync`
//!
//! Nothing here is bounded by `Send` or `Sync`, and nothing may become so. One engine is driven
//! by one thread. Hermes — React Native's JavaScript engine — is single-threaded, so a `Send`
//! bound here would force every binding to wrap its storage handle in a mutex to satisfy a
//! constraint the design never needed. `tests/single_threaded.rs` holds this to it.

use crate::error::{StorageError, TransportError};
use crate::outbox::OutboxEntry;
use crate::storage::{StorageOp, TxOutcome};
use crate::types::{RequestId, Timestamp};
use crate::wire::WireRequest;
use credsync_protocol::{Cursor, EntityId, EntityName, HexString, RowVersion, ScopeId};

/// Reads the wall clock.
///
/// The only way the core learns what time it is. There is no `Instant::now` anywhere in this
/// crate and CI greps the source to keep it that way, because a single hidden clock read destroys
/// deterministic replay *silently* — the simulator keeps passing while losing the ability to find
/// anything.
///
/// Implementations must be monotonic within a run. A clock that jumps backwards is a device
/// whose user changed the date, which happens, so the engine treats `client_ts` as advisory
/// throughout (`docs/spec.md` §6) rather than assuming this promise holds in the field.
pub trait Clock {
    /// The current time, Unix milliseconds.
    fn now(&self) -> Timestamp;
}

/// Supplies randomness.
///
/// Used for UUIDv7 command ids and for backoff jitter. `docs/spec.md` §2 requires that jitter be
/// *"drawn from the client's seeded entropy source so that simulated runs replay identically"* —
/// which is only true if every byte of randomness in the system comes through here.
///
/// Bytes rather than numbers: a UUIDv7 wants 10 random bytes, jitter wants a bounded integer, and
/// a trait returning `u64` would have the core deriving the other from it in a way each
/// implementation could get subtly different.
pub trait Entropy {
    /// Fills `buf` with random bytes.
    fn fill(&mut self, buf: &mut [u8]);
}

/// Commits a batch of writes atomically, and reports what a row is currently at.
///
/// The batch commits whole or not at all. See [`crate::storage`] for why that is load-bearing
/// rather than merely tidy.
pub trait Storage {
    /// Applies every op in one transaction.
    ///
    /// # An adapter must never report failure after committing
    ///
    /// Whole or not at all, in both directions. Partially applying and reporting success is the
    /// obvious violation; **committing and then reporting failure is the subtle one**, and it is
    /// the more damaging because nothing errors.
    ///
    /// The engine then holds a cursor, digest and outbox that are behind what storage contains. Its
    /// next pull re-fetches changes storage already has, the digest regresses permanently, and the
    /// scope reports divergence on every pull for the rest of its life. Its next push re-sends
    /// entries storage has already resolved, recording a second outcome for one command —
    /// inflating the dead-letter list a user actually reads.
    ///
    /// The window is real on mobile: SQLite can commit and have the process killed before the
    /// return value is observed. In-process that is a kill, and a restarted engine is safe. But a
    /// binding that catches the error across an FFI boundary, or a wrapper that retries, turns a
    /// kill into a *returned error with the engine still alive* — which is the dangerous shape.
    ///
    /// **The engine defends against it anyway.** After any error here it treats its cached state
    /// for the affected scope, and its outbox, as of unknown accuracy and reloads both from
    /// [`scope_state`](Self::scope_state) and [`outbox`](Self::outbox) — because it cannot
    /// distinguish "nothing was written" from "everything was written and I was not told", and the
    /// cost of assuming the worse one is two reads (#55).
    ///
    /// # Errors
    /// Returns [`StorageError::Transient`] when retrying could help and
    /// [`StorageError::Corrupt`] when the local database is unusable. An adapter must not
    /// partially apply and report success — the engine has no way to detect that, and `applied`
    /// disagreeing with the op count is the only signal it gets.
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError>;

    /// The version a live row currently holds, or `None` if no such row is stored.
    ///
    /// Added at CS-7 (D-039). The scope digest is a sum of per-row hashes over
    /// `(entity, entity_id, row_version)`, so replacing a row means *subtracting the old
    /// contribution* before adding the new one — and that is impossible without knowing which
    /// version is being replaced. A tombstone has the same problem: the row being removed must
    /// be subtracted at the version it was actually stored at, not at the version the change
    /// happens to mention.
    ///
    /// The alternative was to have each adapter maintain the digest itself, since it already
    /// knows the row it is overwriting. That was rejected: it would put the subtlest arithmetic
    /// in the system into every port, where a single wrong tombstone silently produces phantom
    /// divergence reports on real devices.
    ///
    /// Keyed by `(entity, entity_id)` with no scope, because the entity registry maps each
    /// entity to exactly one scope (`docs/spec.md` §1) — so the pair already identifies a row
    /// uniquely, and passing a scope would invite the two from disagreeing.
    ///
    /// # Errors
    /// Returns [`StorageError::Corrupt`] if the local database cannot be read. A *missing row*
    /// is `Ok(None)`, not an error: the first time any row arrives it is absent, so treating
    /// that as a failure would make every insert an error.
    fn row_version(
        &self,
        entity: &EntityName,
        entity_id: &EntityId,
    ) -> Result<Option<RowVersion>, StorageError>;

    /// The cursor and digest currently stored for a scope, or `None` if it has never synced.
    ///
    /// # Why the engine needs to re-read what it just wrote
    ///
    /// The engine caches each scope's cursor and digest in memory and advances them only after
    /// `transact` returns `Ok` — which is right, and not enough. An adapter that **commits and
    /// then reports failure** leaves the engine holding state that is stale rather than merely
    /// unadvanced, and the consequence is silent:
    ///
    /// ```text
    ///   transact(ops)  ->  committed, then Err(Transient)
    ///   engine         ->  does not advance its cursor or digest (correct)
    ///   next pull      ->  re-fetches changes storage already holds
    ///   stage_change   ->  reads row_version, which is ALREADY the new value, so
    ///                      digest.update(new, new) is a no-op
    ///                  ->  writes the STALE in-memory digest over the correct stored one
    /// ```
    ///
    /// The rows stay right and the digest regresses permanently, so the scope reports divergence
    /// on every pull for the rest of its life. Nothing in the engine can detect it, because the
    /// layer that lied is the one it would have to ask.
    ///
    /// So after any `StorageError` the engine stops trusting its cached state for that scope and
    /// reloads it here. **An adapter must never report failure after committing** — but this is
    /// cheap, and the failure it prevents is silent and permanent (#55).
    ///
    /// # Errors
    /// Returns [`StorageError::Corrupt`] if the local database cannot be read. A scope that has
    /// never synced is `Ok(None)`, not an error.
    fn scope_state(&self, scope: &ScopeId) -> Result<Option<StoredScope>, StorageError>;

    /// Every outbox entry currently stored, oldest first.
    ///
    /// Read for the same reason as [`scope_state`](Self::scope_state), and it is the same failure
    /// wearing different clothes. After a transaction that committed and reported failure, the
    /// engine's in-memory outbox still holds entries storage has already resolved. Those entries
    /// are pushed again, the server's dedupe table returns the same verdict, and the engine records
    /// a **second** outcome for one command — which `docs/spec.md` §3.3 forbids, and which matters
    /// because the user's dead-letter list is built by counting those records.
    ///
    /// Found by the simulator once a fault existed that lies about a commit *without* killing the
    /// device. The paired kill-and-restart version could never reach it: a restarted engine reloads
    /// everything and never consults the state the lie invalidated (#55).
    ///
    /// # Errors
    /// Returns [`StorageError::Corrupt`] if the local database cannot be read. An empty outbox is
    /// `Ok(vec![])`, not an error.
    fn outbox(&self) -> Result<Vec<OutboxEntry>, StorageError>;
}

/// A scope's persisted sync position, as storage holds it.
///
/// Deliberately the stored form — a `HexString` digest rather than a [`ScopeDigest`] — because that
/// is what an adapter has. Converting is the engine's job, and an adapter that had to reconstruct
/// the digest type would be reconstructing the arithmetic D-039 kept out of every port.
///
/// [`ScopeDigest`]: credsync_protocol::ScopeDigest
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScope {
    /// The last `seq` applied for this scope.
    pub cursor: Cursor,
    /// The digest recorded alongside it, 32 lowercase hex characters.
    pub digest: HexString,
}

/// Measures how large a payload will be once compressed.
///
/// Added at CS-8 (D-042). `docs/spec.md` §2 makes byte budgets **compressed-size budgets**, and
/// negotiates Brotli or gzip on the wire — so the core cannot know which algorithm will be used
/// and must not guess. A batch sized against the wrong assumption either wastes a round trip on
/// an under-filled request or gets refused for being over budget, and on a 2G link both are
/// expensive.
///
/// The caller supplies the compressor its transport actually uses, so the number the engine
/// budgets against is the number that goes on the wire. The simulator supplies a deterministic
/// fake, exactly as for the other four traits.
///
/// Implementations must be **pure and deterministic**: the same bytes always yield the same
/// length. A compressor consulting a clock, a thread pool, or an adaptive dictionary that varies
/// between calls would make batch composition differ between two runs of the same seed, and
/// deterministic replay would be gone.
pub trait Compressor {
    /// The length `bytes` would occupy compressed.
    ///
    /// Implementations may compress for real or estimate, but an estimate that under-reports
    /// produces over-budget requests. When in doubt, over-report: a slightly under-filled batch
    /// costs one extra round trip, while a refused one costs that plus a retry.
    fn compressed_len(&self, bytes: &[u8]) -> usize;
}

/// Hands a request to the network.
///
/// Fire-and-forget by design: `enqueue` returns a handle immediately and the answer arrives later
/// as [`Event::TransportResponse`](crate::Event::TransportResponse). Blocking here would make the
/// core wait on a network that, for its users, routinely does not answer at all.
pub trait Transport {
    /// Queues a request and returns the handle its response will quote.
    ///
    /// Handles must be unique within a run. Reusing one lets a late response from a dead request
    /// be mistaken for the answer to a live one — which on a link that duplicates and reorders is
    /// not a hypothetical.
    ///
    /// # Errors
    /// Returns [`TransportError::Unreachable`] when the request cannot be queued at all. Note
    /// that a request failing *after* being queued is not an error here: it arrives as a failed
    /// [`Event::TransportResponse`](crate::Event::TransportResponse) carrying its handle, so the
    /// engine can tie the failure to what caused it.
    fn enqueue(&mut self, req: WireRequest) -> Result<RequestId, TransportError>;
}
