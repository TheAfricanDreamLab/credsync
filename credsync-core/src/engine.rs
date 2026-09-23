//! The state machine itself — at CS-6, its shape and nothing more.
//!
//! State transitions arrive at CS-7 (#8) onward. There is deliberately no `handle` method yet: a
//! `handle` that accepted an [`Event`](crate::Event) and did nothing would compile, look
//! finished, and quietly swallow every event fed to it. An absent method is a compile error at
//! the call site, which is the failure anyone would rather have.

use crate::apply::{self, Applied, ApplyError};
use crate::effect::Effect;
use crate::scope::ScopeState;
use crate::storage::StorageOp;
use crate::traits::{Clock, Entropy, Storage, Transport};
use core::fmt;
use credsync_protocol::{Batch, ScopeId};
use std::collections::{BTreeMap, VecDeque};

/// The credSync client state machine.
///
/// # Single-threaded by construction
///
/// No method, field, or bound here mentions `Send` or `Sync`, and none may. One engine is driven
/// by one thread.
///
/// This is not caution about concurrency — it is a constraint the consumers actually impose.
/// Hermes, React Native's JavaScript engine, is single-threaded; a `Send` bound here would force
/// every binding to wrap its SQLite handle in a mutex to satisfy a requirement the design never
/// had. `tests/single_threaded.rs` constructs this engine from four deliberately `!Send` parts,
/// so adding such a bound stops compiling rather than merely becoming regrettable.
///
/// # Why it owns all four traits
///
/// Design v2.1 §4.1 describes storage results arriving as events, which would put `Storage`
/// outside the engine. It is held here instead (D-037): `Storage::transact` answers immediately,
/// so routing its result back through the event queue would mean parking a half-finished apply
/// across a round trip — the exact state that must never be interruptible. `Transport` differs
/// and is answered by [`Event::TransportResponse`](crate::Event::TransportResponse), because a
/// request handed to the network is answered later or never.
pub struct Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    clock: C,
    entropy: E,
    storage: S,
    transport: T,
    /// Queued for the caller to drain. Ordered: effect order is part of the engine's observable
    /// behaviour, and the simulator asserts two runs of one seed produce the same stream.
    effects: VecDeque<Effect>,
    /// Per-scope cursor and digest.
    ///
    /// `BTreeMap`, not `HashMap`. `HashMap`'s iteration order is randomised per process, and
    /// anything that iterates scopes — a sync cycle's request order, a telemetry dump, a
    /// simulator trace — would then differ between two runs of the same seed. Deterministic
    /// replay is the property this entire crate exists to preserve, and it is lost in exactly
    /// this sort of quiet way. The workspace denies `clippy::iter_over_hash_type` for the same
    /// reason.
    scopes: BTreeMap<ScopeId, ScopeState>,
}

impl<C, E, S, T> Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    /// Builds an engine over the four supplied implementations.
    pub const fn new(clock: C, entropy: E, storage: S, transport: T) -> Self {
        Self {
            clock,
            entropy,
            storage,
            transport,
            effects: VecDeque::new(),
            scopes: BTreeMap::new(),
        }
    }

    /// Seeds a scope's cursor and digest from what storage holds.
    ///
    /// Called at startup, once per subscribed scope. Without it the engine treats a scope as
    /// never synced and pulls it from the beginning — correct, but it would re-walk the entire
    /// change log on every app launch.
    pub fn restore_scope(&mut self, scope: ScopeId, state: ScopeState) {
        self.scopes.insert(scope, state);
    }

    /// This client's cursor and digest for a scope, if it has any.
    #[must_use]
    pub fn scope_state(&self, scope: &ScopeId) -> Option<&ScopeState> {
        self.scopes.get(scope)
    }

    /// Applies one pulled batch: validate, stage, commit, then advance.
    ///
    /// # The order of operations is the correctness argument
    ///
    /// 1. **Validate everything first.** A batch that will be refused never reaches storage.
    /// 2. **Stage into one `Vec<StorageOp>`** — every row, then the cursor, then the digest.
    /// 3. **Commit once.** `docs/spec.md` §4: the cursor is persisted in the same transaction as
    ///    the rows it covers, so a process killed at any moment either has all of it or none.
    /// 4. **Advance memory only after the commit returns `Ok`.** A failed transaction leaves the
    ///    engine describing the state the database is actually in, so the batch can simply be
    ///    refetched.
    ///
    /// Reversing 3 and 4 is the classic version of this bug: a cursor advanced in memory, a
    /// commit that fails, and a client that never asks for those changes again. It reports
    /// success forever while missing rows it silently skipped.
    ///
    /// # Errors
    /// Returns [`ApplyError`] if the batch breaks an ordering rule, carries an inconsistent
    /// change, or storage refuses the transaction. In every case **nothing is written and no
    /// state moves**.
    pub fn apply_batch(&mut self, batch: &Batch) -> Result<Applied, ApplyError> {
        let state = self.scopes.get(&batch.scope).copied().unwrap_or_default();

        apply::validate_ordering(batch, state.cursor.get())?;

        // Staged against a copy. If anything below fails, the real digest is untouched.
        let mut digest = state.digest;
        let mut ops: Vec<StorageOp> = Vec::with_capacity(batch.changes.len() + 2);
        // Tracks what earlier changes in THIS batch did, since they have not committed yet and
        // are therefore invisible to `Storage::row_version`. See `apply::stage_change`.
        let mut staged = apply::Staged::new();

        for change in &batch.changes {
            apply::stage_change(&self.storage, change, &mut digest, &mut ops, &mut staged)?;
        }

        // The cursor and digest ride in the same transaction as the rows. Not a convenience:
        // see the method docs, and `docs/spec.md` §4.
        ops.push(StorageOp::SetCursor {
            scope: batch.scope.clone(),
            cursor: batch.next_cursor,
        });
        ops.push(StorageOp::SetScopeDigest {
            scope: batch.scope.clone(),
            digest: digest.to_hex(),
        });

        let outcome = self.storage.transact(&ops)?;
        debug_assert_eq!(
            outcome.applied,
            ops.len(),
            "an adapter reported a partial commit as success"
        );

        // Committed. Only now does anything in memory move.
        self.scopes.insert(
            batch.scope.clone(),
            ScopeState::restored(batch.next_cursor, digest),
        );

        // `docs/spec.md` §5: the client compares after apply. A mismatch is silent divergence —
        // both sides walked the same log and hold different rows. Marking the scope tainted and
        // re-bootstrapping is CS-22 (#23); detecting and reporting it is this slice.
        let diverged = digest.to_hex() != batch.digest;
        if diverged {
            self.emit(Effect::Emit(apply::divergence(batch, &digest)));
        }

        Ok(Applied {
            changes: batch.changes.len(),
            diverged,
        })
    }

    /// Takes the next queued effect, oldest first.
    ///
    /// Returns `None` when the queue is empty, which is the normal resting state — an engine with
    /// nothing to say is an engine with nothing to do.
    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effects.pop_front()
    }

    /// How many effects are waiting.
    #[must_use]
    pub fn pending_effects(&self) -> usize {
        self.effects.len()
    }
}

/// The handles the transitions will reach for.
///
/// `dead_code` is allowed here, and only here, because CS-6 is explicitly a skeleton: the issue
/// puts every state transition in CS-7 (#8) and later. The accessors exist now so the four
/// implementations are *stored* now, which is what fixes the struct's bounds — and fixing them
/// now is the point of the slice, since `tests/single_threaded.rs` asserts against exactly these
/// bounds.
///
/// If this attribute is still here once transitions exist, it is stale and should go.
#[allow(dead_code)]
impl<C, E, S, T> Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    /// Queues an effect for the caller.
    ///
    /// Crate-private: effects are produced by transitions, never by a caller reaching in.
    pub(crate) fn emit(&mut self, effect: Effect) {
        self.effects.push_back(effect);
    }

    /// The injected clock.
    pub(crate) const fn clock(&self) -> &C {
        &self.clock
    }

    /// The injected entropy source.
    pub(crate) fn entropy_mut(&mut self) -> &mut E {
        &mut self.entropy
    }

    /// The injected storage.
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    /// The injected transport.
    pub(crate) fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
}

// Written by hand rather than derived: `#[derive(Debug)]` would add `C: Debug` and three more
// bounds, so a caller whose SQLite handle is not `Debug` could not debug-print the engine. The
// four implementations are also the least interesting thing about it — what a reader wants is
// how much work is outstanding.
impl<C, E, S, T> fmt::Debug for Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("pending_effects", &self.effects.len())
            .finish_non_exhaustive()
    }
}
