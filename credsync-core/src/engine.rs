//! The state machine itself.
//!
//! Shape at CS-6, the pull apply path at CS-7, the outbox at CS-8.
//!
//! There is still deliberately no `handle` method. Event routing arrives with the slice that
//! needs the full sync loop; a `handle` matching only the events implemented so far, while
//! silently ignoring the rest, would compile, look finished, and swallow everything it did not
//! recognise. An absent method is a compile error at the call site, which is the failure anyone
//! would rather have.

use crate::apply::{self, Applied, ApplyError};
use crate::effect::Effect;
use crate::outbox::{OutboxEntry, OutboxError, Resolution, Resolved};
use crate::registry::Registry;
use crate::scope::ScopeState;
use crate::storage::StorageOp;
use crate::traits::{Clock, Compressor, Entropy, Storage, Transport};
use core::fmt;
use credsync_protocol::{
    Batch, Command, CommandId, ConflictClass, ProtocolVersion, PushRequest, PushResponse, Reason,
    ScopeId, Status, canonical, limits,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
/// had. `tests/single_threaded.rs` constructs this engine from five deliberately `!Send` parts,
/// so adding such a bound stops compiling rather than merely becoming regrettable.
///
/// # Why it owns all five traits
///
/// Design v2.1 §4.1 describes storage results arriving as events, which would put `Storage`
/// outside the engine. It is held here instead (D-037): `Storage::transact` answers immediately,
/// so routing its result back through the event queue would mean parking a half-finished apply
/// across a round trip — the exact state that must never be interruptible. `Transport` differs
/// and is answered by [`Event::TransportResponse`](crate::Event::TransportResponse), because a
/// request handed to the network is answered later or never.
///
/// [`Compressor`] joined them at CS-8 (D-042). `docs/spec.md` §2 makes byte budgets
/// compressed-size budgets and negotiates the algorithm on the wire, so the core must be told how
/// large a batch will actually be rather than guess.
pub struct Engine<C, E, S, T, Z>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
    Z: Compressor,
{
    clock: C,
    entropy: E,
    storage: S,
    transport: T,
    compressor: Z,
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
    /// Commands waiting for the server to say what happened to them.
    ///
    /// A queue, not a set: `docs/spec.md` §4 pushes before it pulls, and commands are sent in the
    /// order they were written so a later edit never reaches the host ahead of the earlier one it
    /// depends on.
    outbox: VecDeque<OutboxEntry>,
    /// What the host declared about its entities and commands.
    ///
    /// Data, not a trait: the registry is a table the host fills in, and nothing about it touches
    /// the outside world. Empty by default, which means an engine accepts no commands until the
    /// host declares some — the safe direction to fail.
    registry: Registry,
}

impl<C, E, S, T, Z> Engine<C, E, S, T, Z>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
    Z: Compressor,
{
    /// Builds an engine over the five supplied implementations.
    pub fn new(clock: C, entropy: E, storage: S, transport: T, compressor: Z) -> Self {
        Self {
            clock,
            entropy,
            storage,
            transport,
            compressor,
            effects: VecDeque::new(),
            scopes: BTreeMap::new(),
            outbox: VecDeque::new(),
            registry: Registry::default(),
        }
    }

    /// The entity and command registry.
    #[must_use]
    pub const fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The registry, for the host to declare entities and commands into.
    pub const fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
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
            // The class comes from the registry, so append-only enforcement is driven by the
            // host's declaration rather than by anything hard-coded here.
            let class = self.registry.class_of(&change.entity);
            apply::stage_change(
                &self.storage,
                change,
                class,
                &mut digest,
                &mut ops,
                &mut staged,
            )?;
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

    /// Queues a client write.
    ///
    /// `docs/spec.md` §3.3 and D-004: client writes are **commands, never row writes**. The entry
    /// is persisted before this returns, so a crash immediately afterwards still finds it waiting.
    ///
    /// The registry is consulted **first**. `docs/spec.md` §6 makes institution truth — grades,
    /// scores, schedules, statuses — pull-only, *"refused by the registry, not by convention"*.
    /// Refusing here rather than at the server matters: a client that could queue such a command
    /// could change its own grade, and a modified client simply would not ask the server's
    /// permission.
    ///
    /// # Errors
    /// Returns [`OutboxError::Registry`] if the command is unregistered or targets a
    /// server-authoritative entity, and [`OutboxError::Storage`] if the entry could not be
    /// persisted. Nothing is queued in memory in either case — an in-memory entry whose
    /// persistence failed is a write the user was told was saved and which will vanish at the
    /// next launch.
    pub fn enqueue(&mut self, entry: OutboxEntry) -> Result<(), OutboxError> {
        self.registry.check_command(&entry.command)?;

        self.storage.transact(&[StorageOp::EnqueueCommand {
            command: entry.command.clone(),
            schema_version: entry.schema_version,
        }])?;
        self.outbox.push_back(entry);
        Ok(())
    }

    /// Restores queued commands from storage at startup.
    ///
    /// Order matters and is the caller's to preserve: commands are pushed in the order they were
    /// written, so a later edit never reaches the host before the earlier one it depends on.
    pub fn restore_outbox(&mut self, entries: impl IntoIterator<Item = OutboxEntry>) {
        self.outbox.extend(entries);
    }

    /// How many commands are waiting.
    #[must_use]
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Whether a command is still queued.
    #[must_use]
    pub fn outbox_contains(&self, id: CommandId) -> bool {
        self.outbox.iter().any(|e| e.id() == id)
    }

    /// Builds the next push request, filling it up to the compressed byte budget.
    ///
    /// Returns `None` when the outbox is empty.
    ///
    /// # How the budget is applied
    ///
    /// `docs/spec.md` §2 makes byte budgets **compressed-size budgets**, so each candidate batch
    /// is encoded canonically and measured through the injected [`Compressor`] — never counted in
    /// rows. Highly compressible payloads therefore travel in larger batches, which is the whole
    /// point on a link where bytes are the scarce resource rather than round trips.
    ///
    /// Three limits bind, whichever comes first: the compressed budget, the 256-entry cap and the
    /// 1 MB uncompressed cap from `docs/spec.md` §2.1. The last two are the server's limits, so
    /// exceeding them produces a refusal rather than a slow request.
    ///
    /// **One command always goes, even if it exceeds the budget alone.** A single oversized entry
    /// that could never fit would otherwise wedge the outbox permanently — nothing sent, nothing
    /// resolved, and every later write stuck behind it. The same rule `docs/spec.md` §2 states for
    /// pull batches: *"a single change larger than the budget is still delivered alone rather
    /// than stalling the cursor."*
    ///
    /// # Errors
    /// Returns [`OutboxError::Encoding`] if a queued command cannot be encoded, which would mean
    /// a value that passed validation on the way in has since become unrepresentable.
    pub fn build_push(
        &self,
        protocol: ProtocolVersion,
        budget_bytes: usize,
    ) -> Result<Option<PushRequest>, OutboxError> {
        if self.outbox.is_empty() {
            return Ok(None);
        }

        let mut chosen: Vec<Command> = Vec::new();

        // The candidate batch's canonical bytes, grown in place. Starts as the empty array.
        //
        // Each command is encoded **once** and appended; the previous implementation cloned the
        // chosen list and re-encoded all of it on every iteration, which is O(n^2) bytes of JSON
        // serialisation to fill a batch of n (#53). That cost landed exactly where there is least
        // budget for it: `docs/spec.md` §3.2's headline case is a three-week-offline device, which
        // is a device with a *large* outbox, on the worst link, with the slowest processor.
        //
        // Splicing in place rather than building a fresh candidate each time is what keeps it
        // linear — a rejected command is rolled back by truncating, which is O(1).
        let mut buf: Vec<u8> = vec![b'[', b']'];

        for entry in &self.outbox {
            if chosen.len() >= limits::COMMANDS_MAX_COUNT {
                break;
            }

            let one = canonical::to_vec(&entry.command).map_err(|_| OutboxError::Encoding)?;

            // Splice `one` in before the closing bracket: `[a,b]` + c -> `[a,b,c]`.
            let rollback = buf.len();
            buf.pop();
            if !chosen.is_empty() {
                buf.push(b',');
            }
            buf.extend_from_slice(&one);
            buf.push(b']');

            let too_many_bytes = buf.len() > limits::COMMANDS_MAX_TOTAL_BYTES;
            let over_budget = self.compressor.compressed_len(&buf) > budget_bytes;

            // One command always goes, even alone over budget: see the method docs. Otherwise a
            // single oversized entry wedges the outbox permanently.
            if (too_many_bytes || over_budget) && !chosen.is_empty() {
                buf.truncate(rollback - 1);
                buf.push(b']');
                break;
            }

            chosen.push(entry.command.clone());
        }

        Ok(Some(PushRequest {
            protocol,
            commands: chosen,
        }))
    }

    /// Applies the server's verdicts, resolving each command it answered.
    ///
    /// # Idempotent by construction
    ///
    /// A result for a command that is no longer queued is ignored. Replays are ordinary here —
    /// a retried push after a timeout returns the same results a second time — so re-resolving
    /// must change nothing rather than double-count or error.
    ///
    /// # What is deliberately *not* done
    ///
    /// A command with **no** result stays queued. The server answered about others and said
    /// nothing about this one, which is not permission to discard it. This is the single most
    /// important line in the outbox: silent loss is a protocol violation, not a tradeoff (D-009).
    ///
    /// # Errors
    /// Returns [`OutboxError::Storage`] if the resolutions could not be committed. Nothing is
    /// removed from the in-memory outbox in that case, so the push is simply retried.
    pub fn apply_results(&mut self, response: &PushResponse) -> Result<Resolved, OutboxError> {
        let mut ops: Vec<StorageOp> = Vec::new();
        let mut resolutions: Vec<(CommandId, Resolution)> = Vec::new();
        let mut unknown = 0usize;

        // Ids this response has already answered. The in-memory outbox is not pruned until the
        // commit succeeds, so `outbox_contains` alone would let a response naming the same
        // command twice resolve it twice.
        //
        // That is not cosmetic. Two verdicts for one command produce two records, and if they
        // disagree the stored outcome depends on write order: a dead letter recorded for a
        // command the host actually applied, or an "applied" masking a real rejection the user
        // needed to see. First answer wins; later repeats are noise.
        //
        // Found by `tests/outbox_property.rs`, and it is the same shape as the CS-7 staging bug
        // (D-041) — logic that reads state its own in-progress batch is about to change. See
        // `.claude/skills/rust-sans-io/SKILL.md`, "Staging a transaction: read your own writes".
        let mut answered: BTreeSet<CommandId> = BTreeSet::new();

        for result in &response.results {
            // Found once, up front: the entry carries the command, which is what both the
            // recovered-draft decision and the registry lookup need.
            let Some(entry) = self.outbox.iter().find(|e| e.id() == result.id) else {
                // Already resolved, or never ours. Either way there is nothing to do, which is
                // what makes a replayed response harmless.
                unknown += 1;
                continue;
            };
            if !answered.insert(result.id) {
                // Answered earlier in this same response.
                unknown += 1;
                continue;
            }

            let resolution = match result.status {
                Status::Applied => Resolution::Applied {
                    server_seq: result.server_seq,
                },
                Status::Superseded => {
                    // The server applied last-write-wins and this edit lost. The user's text is
                    // in this command's payload and exists nowhere else on the device once the
                    // entry leaves the outbox -- so it is preserved in the SAME transaction that
                    // resolves it (`docs/spec.md` §6: silent loss is a protocol violation, not a
                    // tradeoff).
                    //
                    // Only for owner drafts. An append-only stream has no draft to recover -- its
                    // entries are never replaced -- and a server-authoritative entity could never
                    // have had a command queued against it in the first place.
                    if let Some(entity) = self.registry.target_of(&entry.command.name)
                        && self.registry.class_of(entity) == Some(ConflictClass::OwnerDraft)
                    {
                        ops.push(StorageOp::SaveRecoveredDraft {
                            entity: entity.clone(),
                            command_id: result.id,
                            payload: entry.command.payload.clone(),
                        });
                    }
                    Resolution::Superseded
                }
                Status::Rejected => {
                    // `CommandResult`'s decoder enforces that a rejection carries a reason, but a
                    // response built programmatically can still omit it. Refusing the whole batch
                    // over one malformed result would strand every other command in it, so the
                    // entry is dead-lettered with a stated fallback instead — dead-lettering with
                    // an unhelpful reason is recoverable, dropping the entry is not.
                    let reason = result.reason.clone().unwrap_or_else(|| {
                        Reason::new("Rejected by the server without a stated reason.")
                            .unwrap_or_else(|_| unreachable!("literal is a valid reason"))
                    });
                    Resolution::DeadLettered { reason }
                }
            };

            ops.push(StorageOp::ResolveCommand {
                id: result.id,
                resolution: resolution.clone(),
            });
            resolutions.push((result.id, resolution));
        }

        if ops.is_empty() {
            return Ok(Resolved {
                resolutions,
                unknown,
            });
        }

        // Every resolution commits together, and only then does the in-memory queue shrink.
        // Reversed, a failed commit would leave commands gone from memory and still pending in
        // storage: resurrected at the next launch and pushed again, which the server would dedupe
        // -- but the user's dead-letter would have silently vanished in the meantime.
        self.storage.transact(&ops)?;

        for (id, _) in &resolutions {
            self.outbox.retain(|e| e.id() != *id);
        }

        Ok(Resolved {
            resolutions,
            unknown,
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
/// `dead_code` is allowed here because `Clock`, `Entropy` and `Transport` have no caller yet:
/// time enters through `Event::Tick`, entropy is first needed when the client mints its own
/// command ids, and requests are handed to the transport by the sync loop. `Storage` and
/// `Compressor` are used directly by `apply_batch` and `build_push` and need no accessor.
///
/// Each remaining accessor should lose this allowance as its slice arrives. If the attribute
/// outlives all three, it is stale and should go.
#[allow(dead_code)]
impl<C, E, S, T, Z> Engine<C, E, S, T, Z>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
    Z: Compressor,
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
impl<C, E, S, T, Z> fmt::Debug for Engine<C, E, S, T, Z>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
    Z: Compressor,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("pending_effects", &self.effects.len())
            .field("outbox", &self.outbox.len())
            .finish_non_exhaustive()
    }
}
