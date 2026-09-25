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
use crate::effect::{Effect, Telemetry};
use crate::migrate::{MigrationError, Migrations};
use crate::outbox::{OutboxEntry, OutboxError, Resolution, Resolved};
use crate::registry::Registry;
use crate::scope::{ScopeHealth, ScopeState};
use crate::storage::StorageOp;
use crate::traits::{Clock, Compressor, Entropy, Storage, Transport};
use core::fmt;
use credsync_protocol::{
    Batch, BootstrapResponse, Change, Command, CommandId, ConflictClass, EntityId, EntityName,
    ForcedUpgrade, Payload, ProtocolVersion, PushRequest, PushResponse, Reason, SchemaVersion,
    ScopeDigest, ScopeId, Snapshot, Status, canonical, limits,
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
    /// The host's registered up-migrations. `docs/spec.md` §7.
    migrations: Migrations,
    /// Scopes whose cached cursor and digest may no longer match storage.
    ///
    /// Set whenever `transact` returns an error, because a failure does not say *which* failure: an
    /// adapter that committed and then reported failure leaves this engine holding stale state
    /// rather than merely unadvanced state, and it cannot tell the two apart. See
    /// [`Storage::scope_state`] for what that costs if nobody reloads (#55).
    suspect: BTreeSet<ScopeId>,
    /// Set when the in-memory outbox may no longer match storage.
    ///
    /// Not keyed by scope, because the outbox is not: one failed transaction makes the whole queue
    /// of unknown accuracy. See [`Storage::outbox`] for the second outcome it otherwise produces.
    outbox_suspect: bool,
    /// Per-scope divergence state. `docs/spec.md` §5.
    ///
    /// Keyed by scope so a tainted scope cannot stop the others syncing, which the spec requires
    /// directly: *"A tainted scope does not block others."*
    health: BTreeMap<ScopeId, ScopeHealth>,
    /// Set when the server has refused this client's protocol version.
    ///
    /// While set, nothing is pushed: the server has already said it will not accept this version,
    /// so sending would burn battery and bandwidth to be refused again. The outbox is untouched —
    /// see `on_upgrade_required` for why that is the whole point.
    upgrade_required: Option<ForcedUpgrade>,
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
            migrations: Migrations::new(),
            upgrade_required: None,
            health: BTreeMap::new(),
            suspect: BTreeSet::new(),
            outbox_suspect: false,
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

    /// The registered schema migrations.
    #[must_use]
    pub const fn migrations(&self) -> &Migrations {
        &self.migrations
    }

    /// The migrations, for the host to register up-migrations into.
    ///
    /// `docs/spec.md` §7: credSync applies the host's registered up-migrations; it cannot invent
    /// them, because it does not know what the host's documents mean.
    pub const fn migrations_mut(&mut self) -> &mut Migrations {
        &mut self.migrations
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
        // Digests are compared only on the **last** batch of a walk.
        //
        // The server's `digest` covers the scope as it stands now, not as it stood at the batch's
        // `next_cursor`. A client eight rows into a twenty-row backlog therefore computes a digest
        // over eight rows and disagrees — correctly, and for a reason that has nothing to do with
        // divergence. Comparing mid-walk reports a mismatch on every batch but the last.
        //
        // Before CS-22 that was noisy telemetry. With self-healing it is worse: each false
        // mismatch taints the scope, triggers a re-bootstrap, and escalates a perfectly healthy
        // scope to `Unhealable`. Found by the simulator doing an ordinary catch-up.
        //
        // This is the same rule `apply_bootstrap` already follows for its pages, arrived at from
        // the other direction.
        self.apply_changes(batch, !batch.has_more, false)
    }

    /// Applies one page of a bootstrap. `docs/spec.md` §3.1.
    ///
    /// Bootstrap is the compacted log, so this is the same work as [`apply_batch`](Self::apply_batch)
    /// — same ordering rules, same staging, same single transaction, same cursor-with-rows
    /// guarantee. It is a separate method for exactly one reason.
    ///
    /// # Divergence is not judged until the last page
    ///
    /// The `digest` on a bootstrap response is the server's digest for the **whole scope**. A
    /// client that has applied page one of four holds a quarter of the rows and will not match it,
    /// and that is not divergence — it is a bootstrap in progress.
    ///
    /// Comparing on every page would report divergence on every partial bootstrap, and a detector
    /// that fires constantly during normal operation is a detector somebody switches off. So the
    /// comparison happens only when `has_more` is false.
    ///
    /// # Errors
    /// Returns [`ApplyError`] if the page breaks an ordering rule, carries an inconsistent change,
    /// or storage refuses the transaction. Nothing is written and no state moves.
    pub fn apply_bootstrap(&mut self, response: &BootstrapResponse) -> Result<Applied, ApplyError> {
        let batch = Batch {
            scope: response.scope.clone(),
            changes: response.changes.clone(),
            next_cursor: response.next_cursor,
            has_more: response.has_more,
            checksum: response.checksum.clone(),
            digest: response.digest.clone(),
        };
        // `rebuilding`: a bootstrap page *is* the rebuild, so a divergence on its final page is a
        // heal that failed, which is what the escalation counts. A divergence on an ordinary pull
        // of an already-tainted scope is the same fault observed again, and must not count.
        self.apply_changes(&batch, !response.has_more, true)
    }

    /// The shared body of [`apply_batch`](Self::apply_batch) and
    /// [`apply_bootstrap`](Self::apply_bootstrap).
    ///
    /// `compare_digest` is false only for a bootstrap page that is not the last one.
    fn apply_changes(
        &mut self,
        batch: &Batch,
        compare_digest: bool,
        rebuilding: bool,
    ) -> Result<Applied, ApplyError> {
        // A previous transaction failed on this scope, so the cached cursor and digest are of
        // unknown accuracy: the adapter may have committed and then reported failure, which leaves
        // them stale rather than merely unadvanced. Re-read the truth before touching it.
        self.refresh_if_suspect(&batch.scope)?;

        let state = self.scopes.get(&batch.scope).copied().unwrap_or_default();

        apply::validate_ordering(batch, state.cursor.get())?;

        // Staged against a copy. If anything below fails, the real digest is untouched.
        let mut digest = state.digest;
        let mut ops: Vec<StorageOp> = Vec::with_capacity(batch.changes.len() + 2);
        // Tracks what earlier changes in THIS batch did, since they have not committed yet and
        // are therefore invisible to `Storage::row_version`. See `apply::stage_change`.
        let mut staged = apply::Staged::new();

        // Rows that arrived under a schema this app cannot reach, set aside rather than applied.
        // Reported after the commit: a row is not quarantined until the transaction holding it
        // actually lands.
        let mut quarantined: Vec<(EntityName, EntityId, SchemaVersion, String)> = Vec::new();

        for change in &batch.changes {
            // The class comes from the registry, so append-only enforcement is driven by the
            // host's declaration rather than by anything hard-coded here.
            let class = self.registry.class_of(&change.entity);

            // `docs/spec.md` §7: the client applies registered up-migrations to local rows. A
            // change already at this app's version is the common case and costs nothing.
            match self.migrated_change(change) {
                Ok(None) => apply::stage_change(
                    &self.storage,
                    change,
                    class,
                    &mut digest,
                    &mut ops,
                    &mut staged,
                )?,
                Ok(Some(migrated)) => apply::stage_change(
                    &self.storage,
                    &migrated,
                    class,
                    &mut digest,
                    &mut ops,
                    &mut staged,
                )?,
                Err(e) => {
                    // Set aside, never discarded and never half-migrated. See `stage_quarantine`.
                    let reason = e.to_string();
                    apply::stage_quarantine(
                        &self.storage,
                        change,
                        &mut digest,
                        &mut ops,
                        &mut staged,
                        &reason,
                    )?;
                    quarantined.push((
                        change.entity.clone(),
                        change.entity_id.clone(),
                        change.schema_version,
                        reason,
                    ));
                }
            }
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

        // Divergence is known *before* the commit: the digest is fully computed above, and the
        // server's is on the batch. So the record rides in the same transaction as the state it
        // describes, rather than in a second write afterwards.
        //
        // That matters twice over. `docs/spec.md` §4 requires the cursor to commit with the rows it
        // covers, and a second transaction would mean a process killed between them had advanced
        // its cursor while forgetting that the scope is broken — carrying on against a base it had
        // already decided not to trust. It also keeps one apply to one transaction, which the
        // adapter tests assert directly.
        let health = self.scope_health(&batch.scope);
        let diverged = compare_digest && digest.to_hex() != batch.digest;

        // **One divergence per trust episode, not one per pull.**
        //
        // A tainted scope disagrees on every subsequent pull: the incremental digest is still
        // wrong and stays wrong until it is rebuilt. Counting each of those reached the escalation
        // threshold after three *pulls* and zero rebuilds — which contradicts what `Unhealable`
        // says it means — and then re-emitted `ScopeUnhealable` on every pull thereafter, flooding
        // the one signal that is meant to reach a person.
        //
        // So a divergence counts when the scope was trusted, or when it is the final page of a
        // rebuild. That second case is a heal that failed, which is exactly what is being counted.
        let counts =
            diverged && (health.is_healthy() || (rebuilding && health.needs_rebootstrap()));
        let escalated = counts.then(|| {
            let attempts = health.attempts().saturating_add(1);
            ops.push(StorageOp::RecordDivergence {
                scope: batch.scope.clone(),
                attempts,
                healed: false,
            });
            attempts
        });

        // A rebuild that agrees, recorded durably in the same transaction. Held only in memory, a
        // restart would read the scope as tainted and clear and re-download it on every launch.
        let heals = compare_digest && !diverged && health.needs_rebootstrap();
        if heals {
            ops.push(StorageOp::RecordDivergence {
                scope: batch.scope.clone(),
                attempts: health.attempts(),
                healed: true,
            });
        }

        let outcome = match self.storage.transact(&ops) {
            Ok(outcome) => outcome,
            Err(e) => {
                // Whatever went wrong, this engine no longer knows whether the write landed. The
                // next apply for this scope reloads rather than trusting memory, and so does the
                // next thing that touches the outbox.
                self.suspect.insert(batch.scope.clone());
                self.outbox_suspect = true;
                return Err(e.into());
            }
        };
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

        // Reported only now, for the same reason: a transaction that failed quarantined nothing,
        // and telling the host otherwise would have it surface a row that was never set aside.
        for (entity, entity_id, schema_version, reason) in quarantined {
            self.emit(Effect::Emit(Telemetry::RowQuarantined {
                entity,
                entity_id,
                schema_version,
                reason,
            }));
        }

        // `docs/spec.md` §5: the client compares after apply. A mismatch is silent divergence —
        // both sides walked the same log and hold different rows. Marking the scope tainted and
        // re-bootstrapping is CS-22 (#23); detecting and reporting it is this slice.
        if let Some(attempts) = escalated {
            self.emit(Effect::Emit(apply::divergence(batch, &digest)));
            self.taint(&batch.scope, attempts);
        } else if heals {
            // Agrees again, so it no longer needs rebuilding — but the *count* stays. A scope that
            // diverges, heals and diverges again is the repeated divergence being counted, and
            // forgetting on each apparent success would let that run for ever.
            // `clear_scope_health` is how a host that has investigated forgets it.
            self.health.insert(
                batch.scope.clone(),
                ScopeHealth::Healed {
                    attempts: health.attempts(),
                },
            );
        }

        Ok(Applied {
            changes: batch.changes.len(),
            diverged,
        })
    }

    /// Migrates one change's snapshot to this app's schema version, if it needs it.
    ///
    /// `Ok(None)` means no migration was needed and the original change should be used as-is —
    /// the common case, and worth not cloning a 256 KB snapshot for.
    ///
    /// # Errors
    /// Returns [`MigrationError`] if no chain reaches this app's version or a step failed. The
    /// caller quarantines; nothing is half-migrated.
    fn migrated_change(&self, change: &Change) -> Result<Option<Change>, MigrationError> {
        let Some(target) = self.registry.schema_version_of(&change.entity) else {
            // Unregistered entity: nothing declares what version this app wants, so there is
            // nothing to migrate to. The change is applied as it arrived.
            return Ok(None);
        };
        if change.schema_version == target {
            return Ok(None);
        }
        let Some(snapshot) = change.snapshot.as_ref() else {
            // A tombstone carries no document, so its schema version is irrelevant.
            return Ok(None);
        };

        let migrated = self.migrations.migrate_value(
            &change.entity,
            snapshot.as_value(),
            change.schema_version,
            target,
        )?;

        let snapshot = Snapshot::new(migrated).map_err(|e| MigrationError::Invalid {
            entity: change.entity.clone(),
            detail: e.to_string(),
        })?;

        Ok(Some(Change {
            snapshot: Some(snapshot),
            schema_version: target,
            ..change.clone()
        }))
    }

    /// Reloads a scope's cursor and digest from storage if a previous transaction failed on it.
    ///
    /// # Why a failed write means the cache is stale, not merely behind
    ///
    /// `apply_changes` advances memory only after `transact` returns `Ok`, which is correct and
    /// insufficient. An adapter that commits and *then* reports failure leaves storage ahead of
    /// memory, and the next apply re-fetches changes storage already holds. `stage_change` then
    /// reads a `row_version` that is already the new value, so `digest.update(new, new)` is a
    /// no-op — and the stale in-memory digest is written back over the correct stored one.
    ///
    /// The rows stay right. The digest regresses permanently, and the scope reports divergence on
    /// every pull for the rest of its life: silent, and exactly the shape of failure this project
    /// exists to prevent (#55).
    ///
    /// The contract says an adapter must never do this. This defends anyway, because the engine
    /// cannot distinguish "nothing was written" from "everything was written and I was not told",
    /// and the cost of assuming the worse one is a single read.
    ///
    /// # Errors
    /// Returns [`ApplyError::Storage`] if the reload fails. The scope stays suspect, so the next
    /// attempt tries again rather than proceeding on state it does not trust.
    fn refresh_if_suspect(&mut self, scope: &ScopeId) -> Result<(), ApplyError> {
        if !self.suspect.contains(scope) {
            return Ok(());
        }

        match self.storage.scope_state(scope)? {
            Some(stored) => {
                // A digest storage cannot parse is treated as empty rather than guessed at: an
                // empty digest disagrees with the server on the next pull, which reports divergence
                // and heals (CS-22). A guessed one would agree by accident and never heal.
                let digest = u128::from_str_radix(stored.digest.as_str(), 16)
                    .map_or(ScopeDigest::EMPTY, ScopeDigest::from_raw);
                self.scopes
                    .insert(scope.clone(), ScopeState::restored(stored.cursor, digest));
            }
            // Storage holds nothing for this scope, so neither should memory. Leaving a cached
            // cursor here would have the client resume from a position storage cannot support.
            None => {
                self.scopes.remove(scope);
            }
        }

        // Cleared only after the reload succeeded.
        self.suspect.remove(scope);
        Ok(())
    }

    /// Reloads the outbox from storage if a previous transaction failed.
    ///
    /// The same defence as [`refresh_if_suspect`](Self::refresh_if_suspect), for the queue rather
    /// than the scope. An adapter that committed and reported failure leaves entries resolved in
    /// storage and still queued in memory; pushing them again earns the same verdict from the
    /// server's dedupe table and records a **second** outcome for one command.
    ///
    /// `docs/spec.md` §3.3 gives one result per submitted command, and the user's dead-letter list
    /// is built by counting those records — so a duplicate is a user-visible wrong number, not an
    /// internal tidiness question.
    ///
    /// # Errors
    /// Returns [`OutboxError::Storage`] if the reload fails. The flag stays set, so the next
    /// attempt tries again rather than proceeding on a queue it does not trust.
    fn refresh_outbox_if_suspect(&mut self) -> Result<(), OutboxError> {
        if !self.outbox_suspect {
            return Ok(());
        }
        let stored = self.storage.outbox()?;
        self.outbox = stored.into_iter().collect();
        self.outbox_suspect = false;
        Ok(())
    }

    /// How much this client trusts its copy of a scope. `docs/spec.md` §5.
    #[must_use]
    pub fn scope_health(&self, scope: &ScopeId) -> ScopeHealth {
        self.health
            .get(scope)
            .copied()
            .unwrap_or(ScopeHealth::Healthy)
    }

    /// Every scope waiting to be re-bootstrapped, in name order.
    ///
    /// The sync loop asks this rather than being told, so a tainted scope is picked up after a
    /// restart as readily as in the session that tainted it.
    #[must_use]
    pub fn scopes_needing_rebootstrap(&self) -> Vec<ScopeId> {
        self.health
            .iter()
            .filter(|(_, h)| h.needs_rebootstrap())
            .map(|(scope, _)| scope.clone())
            .collect()
    }

    /// After how many divergences automatic healing gives up.
    ///
    /// Three is a judgement, not a measurement: one is a transient fault worth retrying, two is
    /// bad luck, and a third immediately after rebuilding from the server's own snapshot means
    /// something is systematically wrong. Continuing past that would have a device re-downloading
    /// the same scope forever on a data budget it is paying for.
    pub const MAX_HEAL_ATTEMPTS: u32 = 3;

    /// Records a divergence, escalating once healing has been tried enough times.
    /// Records a divergence in memory, after the transaction carrying it has committed.
    ///
    /// `attempts` is passed in rather than recomputed: it was already decided before the commit,
    /// where it had to be, so that the durable count and the in-memory one can never disagree.
    fn taint(&mut self, scope: &ScopeId, attempts: u32) {
        let health = if attempts >= Self::MAX_HEAL_ATTEMPTS {
            self.emit(Effect::Emit(Telemetry::ScopeUnhealable {
                scope: scope.clone(),
                attempts,
            }));
            ScopeHealth::Unhealable { attempts }
        } else {
            ScopeHealth::Tainted { attempts }
        };
        self.health.insert(scope.clone(), health);
    }

    /// Clears a scope's rows and resets its cursor, ready for a fresh bootstrap.
    ///
    /// # Why the rows go first
    ///
    /// A fresh bootstrap (`after = 0`) carries no tombstones, because a device starting from
    /// nothing has no row to delete (`docs/spec.md` §3.1). So a row this client holds that the
    /// server no longer has would survive a rebuild that did not clear first — and keep the digest
    /// wrong forever, which is the condition being healed.
    ///
    /// The **outbox is untouched**. Those commands have not reached the server yet, and discarding
    /// them to fix a read-side problem would be the cure doing more damage than the disease. They
    /// are replayed after the rebuild, and the server's dedupe table is what stops any that did
    /// arrive from applying twice.
    ///
    /// # Errors
    /// Returns [`ApplyError::Storage`] if the transaction fails. Nothing moves in that case, so
    /// the scope stays tainted and the rebuild is simply retried.
    pub fn begin_rebootstrap(&mut self, scope: &ScopeId) -> Result<(), ApplyError> {
        let ops = vec![
            StorageOp::ClearScope {
                scope: scope.clone(),
                entities: self
                    .registry
                    .entities()
                    .filter(|r| &r.scope == scope)
                    .map(|r| r.entity.clone())
                    .collect(),
            },
            StorageOp::SetCursor {
                scope: scope.clone(),
                cursor: credsync_protocol::Cursor::START,
            },
            StorageOp::SetScopeDigest {
                scope: scope.clone(),
                digest: ScopeDigest::EMPTY.to_hex(),
            },
        ];
        self.storage.transact(&ops)?;

        // Only after the commit. A reset held in memory over a transaction that failed would have
        // the engine re-bootstrapping onto rows it believes are gone.
        self.scopes.insert(scope.clone(), ScopeState::NEW);
        Ok(())
    }

    /// Forgets a scope's divergence history, after somebody has looked at it.
    ///
    /// The count is never cleared automatically, not even by a rebuild that agrees: a scope that
    /// diverges, heals, and diverges again is exactly the repeated divergence the escalation is
    /// for, and resetting on each apparent success would let that run for ever. Clearing is a
    /// deliberate act by a host that has investigated.
    ///
    /// # Errors
    /// Returns [`ApplyError::Storage`] if the write fails; the count is then unchanged.
    pub fn clear_scope_health(&mut self, scope: &ScopeId) -> Result<(), ApplyError> {
        self.storage.transact(&[StorageOp::RecordDivergence {
            scope: scope.clone(),
            attempts: 0,
            healed: false,
        }])?;
        self.health.remove(scope);
        Ok(())
    }

    /// Restores a scope's divergence history from storage, at start-up.
    ///
    /// Without this the escalation counts only the divergences seen since
    /// the process started, and a crash-looping device would rebuild the same scope forever.
    pub fn restore_scope_health(&mut self, scope: ScopeId, attempts: u32, healed: bool) {
        if attempts == 0 {
            self.health.remove(&scope);
            return;
        }
        let health = if healed {
            // Was broken, rebuilt, fine now. Syncs normally, and keeps the history so a scope that
            // breaks again is counted rather than starting over.
            ScopeHealth::Healed { attempts }
        } else if attempts >= Self::MAX_HEAL_ATTEMPTS {
            ScopeHealth::Unhealable { attempts }
        } else {
            ScopeHealth::Tainted { attempts }
        };
        self.health.insert(scope, health);
    }

    /// Records that the server refused this client's protocol version. `docs/spec.md` §7.
    ///
    /// # Nothing is dropped, and that is the entire point
    ///
    /// The spec is explicit: *"The client then queues its outbox and surfaces an upgrade prompt —
    /// it never drops queued work."*
    ///
    /// So this method deliberately does **nothing to the outbox**. It sets a flag and emits a
    /// prompt. The queued commands stay exactly where they are, on disk, waiting for an app version
    /// the server will talk to — which may be days away, and which is precisely when a user would
    /// be least forgiving about losing three weeks of writing.
    ///
    /// The flag stops further pushes. That is not an optimisation: the server has already said it
    /// will not accept this version, so every push until the update lands is a round trip that can
    /// only be refused, on a device that is often paying for its data by the megabyte.
    ///
    /// Pull is left alone. Reading is still useful to a user who cannot write — seeing today's
    /// timetable while being told the app needs updating is a better experience than a blank
    /// screen, and the server refuses the request itself if it disagrees.
    pub fn on_upgrade_required(&mut self, envelope: &ForcedUpgrade) {
        self.upgrade_required = Some(envelope.clone());
        self.emit(Effect::Emit(Telemetry::UpgradeRequired {
            min_protocol: envelope.min_protocol,
            current_protocol: envelope.current_protocol,
            reason: envelope.reason.clone(),
            queued: self.outbox.len(),
        }));
    }

    /// The upgrade the server is asking for, if it has refused this client.
    #[must_use]
    pub const fn upgrade_required(&self) -> Option<&ForcedUpgrade> {
        self.upgrade_required.as_ref()
    }

    /// Clears the forced-upgrade state, after the app has been updated.
    ///
    /// The outbox is untouched here too: whatever was queued when the server refused is what this
    /// newly-updated app now gets to send.
    pub fn upgrade_completed(&mut self) {
        self.upgrade_required = None;
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

        if let Err(e) = self.storage.transact(&[StorageOp::EnqueueCommand {
            command: entry.command.clone(),
            schema_version: entry.schema_version,
        }]) {
            self.outbox_suspect = true;
            return Err(e.into());
        }
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
        &mut self,
        protocol: ProtocolVersion,
        budget_bytes: usize,
    ) -> Result<Option<PushRequest>, OutboxError> {
        // The server has already refused this protocol version, so a push can only be refused
        // again. The outbox is left exactly as it is — see `on_upgrade_required`.
        if self.upgrade_required.is_some() {
            return Ok(None);
        }

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

        // Commands that could not be migrated forward. They stay queued — see below — and are
        // reported after the batch is built so the user is told why nothing is moving.
        let mut held: Vec<(CommandId, SchemaVersion, String)> = Vec::new();

        for entry in &self.outbox {
            if chosen.len() >= limits::COMMANDS_MAX_COUNT {
                break;
            }

            // `docs/spec.md` §7: an upgraded app migrates queued commands forward before pushing.
            // A device offline for three weeks may hold commands authored under a schema the
            // server has moved past, and sending them unmigrated would have them rejected for a
            // reason the user cannot act on.
            //
            // A command that cannot be migrated is **skipped, not dropped**. `build_push` removes
            // nothing from the outbox — only `apply_results` does, and only for an id the server
            // answered — so skipping here leaves the entry exactly where it was, waiting for an
            // app version that knows the migration.
            let command = match Self::migrated_command(&self.registry, &self.migrations, entry) {
                Ok(None) => entry.command.clone(),
                Ok(Some(migrated)) => migrated,
                Err(e) => {
                    held.push((entry.id(), entry.schema_version, e.to_string()));
                    continue;
                }
            };

            let one = canonical::to_vec(&command).map_err(|_| OutboxError::Encoding)?;

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

            chosen.push(command);
        }

        // Held, not dropped. Reported so the user learns why an edit is not saving, rather than
        // watching it sit there silently for the life of the install.
        for (command, authored_under, reason) in held {
            self.emit(Effect::Emit(Telemetry::CommandHeld {
                command,
                authored_under,
                reason,
            }));
        }

        Ok(Some(PushRequest {
            protocol,
            commands: chosen,
        }))
    }

    /// Migrates one queued command's payload up to this app's schema version, if it needs it.
    ///
    /// `Ok(None)` means no migration was needed, so the caller can use the command as-is.
    ///
    /// An associated function rather than a method: `build_push` holds `&self.outbox` across this
    /// call, so taking `&self` here would borrow the engine twice.
    ///
    /// # Errors
    /// Returns [`MigrationError`] if no chain reaches this app's version or a step failed. The
    /// caller holds the entry; nothing is dropped and nothing is half-migrated.
    fn migrated_command(
        registry: &Registry,
        migrations: &Migrations,
        entry: &OutboxEntry,
    ) -> Result<Option<Command>, MigrationError> {
        let Some(entity) = registry.target_of(&entry.command.name) else {
            // An unregistered command cannot be queued (`enqueue` refuses it), so this is only
            // reachable for an outbox restored from storage under a registry that has since
            // dropped the command. Nothing declares a target version, so there is nothing to
            // migrate to.
            return Ok(None);
        };
        let Some(target) = registry.schema_version_of(entity) else {
            return Ok(None);
        };
        if entry.schema_version == target {
            return Ok(None);
        }

        let migrated = migrations.migrate_value(
            entity,
            entry.command.payload.as_value(),
            entry.schema_version,
            target,
        )?;

        let payload = Payload::new(migrated).map_err(|e| MigrationError::Invalid {
            entity: entity.clone(),
            detail: e.to_string(),
        })?;

        Ok(Some(Command {
            payload,
            ..entry.command.clone()
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
        // A previous transaction failed, so entries this engine believes are queued may already be
        // resolved in storage. Answering for them again records a second outcome for one command,
        // which `docs/spec.md` §3.3 forbids and which inflates the user's dead-letter list.
        self.refresh_outbox_if_suspect()?;
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
        if let Err(e) = self.storage.transact(&ops) {
            self.outbox_suspect = true;
            return Err(e.into());
        }

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
