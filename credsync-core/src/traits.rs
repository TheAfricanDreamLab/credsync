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
use crate::storage::{StorageOp, TxOutcome};
use crate::types::{RequestId, Timestamp};
use crate::wire::WireRequest;

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

/// Commits a batch of writes atomically.
///
/// The batch commits whole or not at all. See [`crate::storage`] for why that is load-bearing
/// rather than merely tidy.
pub trait Storage {
    /// Applies every op in one transaction.
    ///
    /// # Errors
    /// Returns [`StorageError::Transient`] when retrying could help and
    /// [`StorageError::Corrupt`] when the local database is unusable. An adapter must not
    /// partially apply and report success — the engine has no way to detect that, and `applied`
    /// disagreeing with the op count is the only signal it gets.
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError>;
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
