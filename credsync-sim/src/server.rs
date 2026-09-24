//! A simulated server: the change log, the dedupe table, and the rules `docs/spec.md` gives them.
//!
//! Deliberately *not* `credsync-server`, which does not exist until M3. This is a model of what a
//! conforming server does — enough to exercise the client against something that behaves like the
//! protocol says, and no more. When the real server lands, the conformance suite is what proves
//! the two agree; until then this is the only server there is.
//!
//! # What it models faithfully, because the client's correctness depends on it
//!
//! - **`seq` is a single `bigserial` shared by every scope**, so one scope's entries are sparse.
//!   A server handing out contiguous per-scope sequences would let a client bug that assumes
//!   contiguity pass every run (D-040).
//! - **Dedupe by command id, recording the outcome.** A replay returns the recorded result
//!   without re-applying (`docs/spec.md` §1).
//! - **A replay whose body was mutated is refused**, not deduped as a success — otherwise the
//!   dedupe table becomes a way to launder tampered commands (`docs/spec.md` §5).
//! - **Server-assigned `row_version` decides last-write-wins**, never `client_ts`.

use crate::rng::Rng;
use credsync_protocol::{
    Batch, Change, Command, CommandId, CommandResult, Cursor, EntityId, EntityName, HexString, Op,
    ProtocolVersion, PullRequest, PullResponse, PushRequest, PushResponse, RowVersion,
    SchemaVersion, ScopeDigest, ScopeId, Seq, Snapshot, Status, payload_checksum,
};
use credsync_server::Compressor;
use std::collections::BTreeMap;

/// Identifies one row: `(entity, entity_id)`. Unique because the registry maps each entity to
/// exactly one scope (`docs/spec.md` §1).
type RowKey = (EntityName, EntityId);

/// Every version one row has held, in `seq` order.
type RowVersions = Vec<(u64, RowVersion)>;

/// Per scope, per row, that row's full version history.
type RowHistory = BTreeMap<ScopeId, BTreeMap<RowKey, RowVersions>>;

/// One entry in the append-only change log.
#[derive(Debug, Clone)]
struct LogEntry {
    seq: Seq,
    change: Change,
    /// Whether a reader can see this entry yet.
    ///
    /// `seq` is allocated when a write *starts*; a row becomes visible when its transaction
    /// *commits*, and those two orders differ. Modelling only the first left the simulator blind
    /// to a whole class of bug — see [`Server::commit_order_guard`].
    visible: bool,
}

/// What the server decided about a command, kept for replays.
#[derive(Debug, Clone)]
struct DedupeEntry {
    /// The checksum of the body that produced this outcome, so a mutated replay is refused.
    checksum: HexString,
    result: CommandResult,
}

/// A deterministic stand-in for Brotli or gzip.
///
/// The budget is a **compressed**-size budget (`docs/spec.md` §2), so `fill_batch` has to be given
/// something that compresses. Real compression would be a poor choice here for two reasons: it is
/// slow enough to matter across thousands of seeds, and it makes the simulator's behaviour depend
/// on a compression library's version rather than on the run seed.
///
/// A fixed ratio is deterministic and monotonic in input length, which is all `fill_batch`
/// actually relies on: more bytes in, never fewer bytes out. Three-to-one is roughly what gzip
/// manages on the repetitive JSON these snapshots are.
///
/// What this deliberately does **not** model is a pathological input that compresses worse than
/// the ratio. That belongs with the fuzzing slice, where a hostile snapshot is the point.
struct SimCompressor;

impl Compressor for SimCompressor {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        bytes.len().div_ceil(3)
    }
}

/// The simulated server.
#[derive(Debug)]
pub struct Server {
    log: Vec<LogEntry>,
    /// Indices into [`log`](Self::log) for each scope, in `seq` order.
    ///
    /// Without this, answering a pull meant filtering the whole log — and with a fortnight of
    /// simulated life the log runs to thousands of entries, so every one of tens of thousands of
    /// pulls rescanned all of it. One seed took three minutes, and a thousand-seed batch would
    /// have taken two days.
    ///
    /// Entries are appended in `seq` order, so each list is sorted and a pull binary-searches for
    /// its cursor instead of scanning.
    by_scope: BTreeMap<ScopeId, Vec<usize>>,
    /// Live rows per scope, keyed by `(entity, entity_id)`.
    rows: BTreeMap<ScopeId, BTreeMap<RowKey, RowVersion>>,
    /// Per row, every version it has held, in `seq` order.
    ///
    /// The invariants ask "what should this device hold at cursor C" after **every step**, for
    /// every device. Answering that by walking the scope's whole log made the check O(log length)
    /// per device per step — the simulator went from 1 second per seed to 12, which turns a
    /// thousand-seed batch from twenty minutes into three and a half hours.
    ///
    /// A scope holds a handful of rows, so this turns the same question into a binary search per
    /// row: O(rows x log history) instead.
    row_history: RowHistory,
    dedupe: BTreeMap<CommandId, DedupeEntry>,
    next_seq: u64,
    /// Which entity each command name writes, mirroring the client's registry.
    command_targets: BTreeMap<String, (EntityName, ScopeId)>,
    /// Commands seen since the last restart. A cold cache loses nothing durable — the dedupe
    /// table is a table, not a cache — but it does lose any in-memory batching state, which is
    /// what `server_restart` exercises.
    pub warm: bool,

    /// Entries that have taken a `seq` and not committed yet, with how many steps remain.
    ///
    /// Models a writer holding a transaction open. Deterministic: the delay comes from the run
    /// seed, never from a real clock or a real thread, so a seed still replays exactly.
    in_flight: Vec<(usize, u32)>,

    /// How long the next write should stay uncommitted. Consumed by the next append.
    hold_next: u32,

    /// Whether pull stops at the first uncommitted entry.
    ///
    /// **This is D-063's fix, and turning it off is how the simulator demonstrates the bug it
    /// now exists to catch.** With it off, a pull hands over every *visible* entry even when a
    /// lower `seq` is still in flight — so a client advances its cursor past a change it will
    /// never be given.
    ///
    /// The client cannot defend itself. `seq` is sparse by design (D-040), so from the client's
    /// side a gap left by an uncommitted write is indistinguishable from a neighbouring scope's
    /// entry. This has to be fixed on the server or not at all.
    pub commit_order_guard: bool,
}

impl Server {
    /// A server with an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: Vec::new(),
            by_scope: BTreeMap::new(),
            rows: BTreeMap::new(),
            row_history: BTreeMap::new(),
            dedupe: BTreeMap::new(),
            next_seq: 1,
            command_targets: BTreeMap::new(),
            warm: true,
            in_flight: Vec::new(),
            hold_next: 0,
            commit_order_guard: true,
        }
    }

    /// Declares which entity and scope a command name writes.
    pub fn register_command(&mut self, name: &str, entity: EntityName, scope: ScopeId) {
        self.command_targets
            .insert(name.to_owned(), (entity, scope));
    }

    /// Makes the next write take its `seq` now and commit `delay` steps later.
    ///
    /// What a writer holding a transaction open looks like from a reader's side.
    pub const fn hold_next_write(&mut self, delay: u32) {
        self.hold_next = if delay == 0 { 1 } else { delay };
    }

    /// Advances every in-flight write, committing those whose delay has run out.
    ///
    /// Commits happen in delay order rather than `seq` order, which is the entire point: a later
    /// `seq` can become visible before an earlier one.
    pub fn advance_commits(&mut self) {
        let mut waiting = Vec::with_capacity(self.in_flight.len());
        for (index, remaining) in std::mem::take(&mut self.in_flight) {
            if remaining <= 1 {
                if let Some(entry) = self.log.get_mut(index) {
                    entry.visible = true;
                }
            } else {
                waiting.push((index, remaining - 1));
            }
        }
        self.in_flight = waiting;
    }

    /// How many writes have taken a `seq` and not yet committed.
    #[must_use]
    pub fn in_flight_writes(&self) -> usize {
        self.in_flight.len()
    }

    /// Restarts with a cold cache.
    ///
    /// The log and the dedupe table survive, because they are durable; only the warm flag drops.
    /// A server restart that lost acknowledged outcomes would not be a fault to simulate, it
    /// would be a server nobody should ship.
    pub fn restart_cold(&mut self) {
        self.warm = false;
    }

    /// The server's own digest for a scope, which the client compares against after applying.
    #[must_use]
    pub fn digest(&self, scope: &ScopeId) -> ScopeDigest {
        // Over what a reader can actually see. A digest including uncommitted writes would report
        // divergence against every client that is correctly up to date.
        let rows = self.rows_at(scope, self.visible_head(scope));
        ScopeDigest::from_rows(rows.iter().map(|(e, i, v)| (e, i, *v)))
    }

    /// The highest `seq` in a scope with every earlier entry committed.
    ///
    /// Where a correct client's cursor can reach. Beyond it sits an uncommitted write, and a
    /// cursor past that is a cursor that skipped it.
    #[must_use]
    pub fn visible_head(&self, scope: &ScopeId) -> u64 {
        let Some(indices) = self.by_scope.get(scope) else {
            return 0;
        };
        let mut head = 0;
        for &i in indices {
            let entry = &self.log[i];
            if !entry.visible {
                break;
            }
            head = entry.seq.get();
        }
        head
    }

    /// How many distinct commands the server has recorded an outcome for.
    ///
    /// The count of *answers given*. Under backpressure this must equal the number of commands the
    /// devices actually got results for — a server that shed load by inventing verdicts would show
    /// more answers here than it ever processed.
    #[must_use]
    pub fn answered_commands(&self) -> usize {
        self.dedupe.len()
    }

    /// Whether the server has recorded an outcome for this command id.
    #[must_use]
    pub fn has_answered(&self, id: &CommandId) -> bool {
        self.dedupe.contains_key(id)
    }

    /// The highest `seq` in the log.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.next_seq - 1
    }

    /// Answers a pull, one batch per requested scope.
    ///
    /// # This calls the real server
    ///
    /// Candidate selection — the log walk, the cursor, the commit-order guard — is what a database
    /// query does, so it is modelled here. **Batching is not modelled.** The compressed byte
    /// budget, the rule that one oversized change is delivered alone rather than stalling the
    /// cursor, `has_more`, and where `next_cursor` lands all come from
    /// [`credsync_server::fill_batch`], the same function `credsyncd` serves from.
    ///
    /// That is the point of CS-18. A simulator checking its own reimplementation of the byte
    /// budget proves the copy correct and says nothing about the code that ships. The two agreed
    /// when this was written, which is exactly when a second implementation looks harmless.
    ///
    /// `row_limit` caps candidates the way the query's `LIMIT` does; `budget_bytes` is the
    /// compressed budget (`docs/spec.md` §2), shrunk by [`crate::pressure::Pressure`] when the
    /// server is shedding.
    #[must_use]
    pub fn pull(
        &self,
        request: &PullRequest,
        row_limit: usize,
        budget_bytes: usize,
    ) -> PullResponse {
        let batches = request
            .scopes
            .iter()
            .map(|sc| {
                let after = sc.cursor.get();
                let indices = self.by_scope.get(&sc.scope);

                // Binary search rather than a scan: the index is in `seq` order, so the first
                // entry past the cursor is found in log time no matter how long the log gets.
                let start = indices.map_or(0, |ix| {
                    ix.partition_point(|&i| self.log[i].seq.get() <= after)
                });
                let available = indices.map_or(0, |ix| ix.len() - start);

                // Visibility, and what a reader is allowed to do about a gap in it.
                //
                // With the guard on (D-063), the walk stops at the first uncommitted entry: a
                // later `seq` is withheld rather than handed over ahead of an earlier one, and
                // arrives on the next pull instead.
                //
                // With it off, every visible entry goes regardless — which is what an unguarded
                // `seq > cursor` query does, and which loses the earlier change permanently once
                // the client advances past it.
                let candidates: Vec<Change> = indices
                    .into_iter()
                    .flat_map(|ix| ix[start..].iter())
                    .map_while(|&i| {
                        let entry = &self.log[i];
                        if entry.visible {
                            Some(Some(entry.change.clone()))
                        } else if self.commit_order_guard {
                            None
                        } else {
                            Some(None)
                        }
                    })
                    .flatten()
                    .take(row_limit)
                    .collect();

                // Whether the row ceiling cut the read short, which `fill_batch` cannot tell from
                // its side — the same reason `db::changes_after` returns `more_beyond`.
                let more_beyond = available > candidates.len();

                let cursor = Cursor::new(after).unwrap_or(Cursor::START);
                let digest = self.digest(&sc.scope).to_hex();

                credsync_server::fill_batch(
                    &sc.scope,
                    cursor,
                    candidates,
                    more_beyond,
                    budget_bytes,
                    &SimCompressor,
                    digest,
                )
                .unwrap_or_else(|_| {
                    // `fill_batch` only fails when a change cannot be encoded, which cannot happen
                    // for a change this server itself built from validated wire types. An empty
                    // batch rather than a panic: a simulator that dies on an impossible branch
                    // tells you less than one that keeps running and lets an invariant catch it.
                    Batch {
                        scope: sc.scope.clone(),
                        changes: Vec::new(),
                        next_cursor: cursor,
                        has_more: more_beyond,
                        checksum: hex_placeholder(),
                        digest: self.digest(&sc.scope).to_hex(),
                    }
                })
            })
            .collect();

        PullResponse {
            protocol: request.protocol,
            batches,
        }
    }

    // `head_for` used to live here: on an empty batch it advanced the cursor to the scope's
    // visible head, so a device on a quiet scope would not re-ask the same empty range.
    //
    // Removed at CS-18, because the real `fill_batch` does not do that — it leaves `next_cursor`
    // exactly where the client sent it when nothing was chosen. Keeping the model's version would
    // have meant the simulator exercising a cursor rule that `credsyncd` does not implement, which
    // is the whole failure mode running the real code is meant to remove. Re-asking a quiet scope
    // costs one empty round trip, which is what polling is.

    /// Every row a client at `cursor` should be holding, with the version it should be at.
    ///
    /// The authoritative answer to "what should this device have", which is what makes durable
    /// *effects* checkable rather than only durable verdicts.
    #[must_use]
    pub fn rows_at(&self, scope: &ScopeId, cursor: u64) -> Vec<(EntityName, EntityId, RowVersion)> {
        let Some(history) = self.row_history.get(scope) else {
            return Vec::new();
        };
        history
            .iter()
            .filter_map(|((entity, entity_id), versions)| {
                // The last entry at or below the cursor, found by binary search rather than by
                // walking: this runs for every row, every device, every step.
                let idx = versions.partition_point(|(seq, _)| *seq <= cursor);
                (idx > 0).then(|| (entity.clone(), entity_id.clone(), versions[idx - 1].1))
            })
            .collect()
    }

    /// The version a row should hold on a client whose cursor is at `cursor`.
    ///
    /// The latest change to that row at or below the cursor — exactly what a correct client ends
    /// up storing, having applied every change in `seq` order.
    #[must_use]
    pub fn row_version_at(
        &self,
        scope: &ScopeId,
        entity: &EntityName,
        entity_id: &EntityId,
        cursor: u64,
    ) -> Option<RowVersion> {
        let versions = self
            .row_history
            .get(scope)?
            .get(&(entity.clone(), entity_id.clone()))?;
        let idx = versions.partition_point(|(seq, _)| *seq <= cursor);
        (idx > 0).then(|| versions[idx - 1].1)
    }

    /// Applies a push, deduping replays and recording every outcome.
    #[must_use]
    pub fn push(&mut self, request: &PushRequest, rng: &mut Rng) -> PushResponse {
        self.push_prefix(request, request.commands.len(), rng)
    }

    /// Answers a push, processing only the first `accepted` commands.
    ///
    /// How a loaded server sheds without lying. The commands beyond `accepted` are **not answered
    /// at all** — not rejected, not deferred with a status, simply absent from `results`. The
    /// client's [`apply_results`] resolves only the ids it is told about, so the rest stay in the
    /// outbox and come back on the next cycle.
    ///
    /// The alternative — answering `rejected` because the server was busy — would tell a student
    /// their work was refused when nothing ever looked at it, and the dedupe table would serve
    /// that back on every retry, permanently. Same rule as D-065, from the other side.
    ///
    /// [`apply_results`]: credsync_core::Engine::apply_results
    pub fn push_prefix(
        &mut self,
        request: &PushRequest,
        accepted: usize,
        rng: &mut Rng,
    ) -> PushResponse {
        let results = request
            .commands
            .iter()
            .take(accepted)
            .map(|c| self.apply_command(c, rng))
            .collect();

        PushResponse {
            protocol: request.protocol,
            results,
        }
    }

    fn apply_command(&mut self, command: &Command, rng: &mut Rng) -> CommandResult {
        let checksum = payload_checksum(&command.payload).unwrap_or_else(|_| hex_placeholder());

        // The REAL dedupe rule, not a model of it (CS-18).
        //
        // `credsync_server::decide` is the same function the Postgres server runs; only the
        // lookup differs, because the database's job is to remember and the rule about what a
        // memory *means* belongs in one place. A second implementation here would have agreed
        // with it on the day it was written, which is exactly when a copy looks harmless.
        let recorded = self
            .dedupe
            .get(&command.id)
            .map(|prior| credsync_server::Recorded {
                checksum: prior.checksum.as_str().to_owned(),
                result: prior.result.clone(),
            });

        match credsync_server::decide(command, recorded.as_ref()) {
            Ok(credsync_server::Decision::Replay(result)) => return result,
            Ok(credsync_server::Decision::Mutated { .. }) => {
                // Same id, different body. Refused as a distinct invalid request rather than
                // deduped as a success, or the dedupe table becomes a way to launder tampered
                // commands (`docs/spec.md` §5).
                let result = CommandResult {
                    id: command.id,
                    status: Status::Rejected,
                    reason: Some(credsync_server::dedupe::mutated_reason()),
                    server_seq: None,
                };
                self.dedupe.insert(
                    command.id,
                    DedupeEntry {
                        checksum,
                        result: result.clone(),
                    },
                );
                return result;
            }
            Ok(credsync_server::Decision::Fresh) => {}
            Err(_) => {
                // Only when the payload cannot be encoded for checksumming, which cannot happen
                // for a command built from validated wire types. Treated as fresh rather than
                // panicking: a simulator that dies on an impossible branch tells you less than one
                // that keeps running and lets an invariant catch the consequence.
            }
        }

        let Some((entity, scope)) = self.command_targets.get(command.name.as_str()).cloned() else {
            let result = CommandResult {
                id: command.id,
                status: Status::Rejected,
                reason: Some(
                    credsync_protocol::Reason::new("Unknown command.")
                        .unwrap_or_else(|_| unreachable!("literal is a valid reason")),
                ),
                server_seq: None,
            };
            self.dedupe.insert(
                command.id,
                DedupeEntry {
                    checksum,
                    result: result.clone(),
                },
            );
            return result;
        };

        // A host would validate business rules here. The simulator models the three outcomes
        // `docs/spec.md` §3.3 defines, weighted so applied is the common case and the other two
        // still occur often enough to be exercised every batch.
        let roll = rng.below(100);
        let result = if roll < 80 {
            let seq = self.append(&scope, &entity, command);
            CommandResult {
                id: command.id,
                status: Status::Applied,
                reason: None,
                server_seq: Some(seq),
            }
        } else if roll < 90 {
            CommandResult {
                id: command.id,
                status: Status::Superseded,
                reason: None,
                server_seq: None,
            }
        } else {
            CommandResult {
                id: command.id,
                status: Status::Rejected,
                reason: Some(
                    credsync_protocol::Reason::new("Submission deadline passed.")
                        .unwrap_or_else(|_| unreachable!("literal is a valid reason")),
                ),
                server_seq: None,
            }
        };

        self.dedupe.insert(
            command.id,
            DedupeEntry {
                checksum,
                result: result.clone(),
            },
        );
        result
    }

    /// Writes one change to the log and updates the server's row set.
    fn append(&mut self, scope: &ScopeId, entity: &EntityName, command: &Command) -> Seq {
        let seq = Seq::new(self.next_seq).unwrap_or_else(|_| unreachable!("seq starts at 1"));
        self.next_seq += 1;

        // The row a command writes is derived from its id, so a device's commands land on a
        // handful of rows and actually collide with each other.
        let entity_id = EntityId::new(format!("row:{}", command.id.as_bytes()[15] % 8))
            .unwrap_or_else(|_| unreachable!("derived id is valid"));

        let rows = self.rows.entry(scope.clone()).or_default();
        let key = (entity.clone(), entity_id.clone());
        let row_version = RowVersion::new(seq.get()).unwrap_or_else(|_| unreachable!("seq >= 1"));
        rows.insert(key, row_version);

        self.push_entry(seq, scope, entity, entity_id, row_version);
        seq
    }

    /// Appends one entry to the log and to its scope's index.
    fn push_entry(
        &mut self,
        seq: Seq,
        scope: &ScopeId,
        entity: &EntityName,
        entity_id: EntityId,
        row_version: RowVersion,
    ) {
        self.by_scope
            .entry(scope.clone())
            .or_default()
            .push(self.log.len());
        self.row_history
            .entry(scope.clone())
            .or_default()
            .entry((entity.clone(), entity_id.clone()))
            .or_default()
            .push((seq.get(), row_version));

        let held = std::mem::take(&mut self.hold_next);
        if held > 0 {
            self.in_flight.push((self.log.len(), held));
        }
        self.log.push(LogEntry {
            seq,
            visible: held == 0,
            change: Change {
                seq,
                entity: entity.clone(),
                entity_id,
                op: Op::Upsert,
                snapshot: Some(
                    Snapshot::new(serde_json::json!({ "v": seq.get() }))
                        .unwrap_or_else(|_| unreachable!("literal is a valid snapshot")),
                ),
                row_version,
                schema_version: SchemaVersion::new(1)
                    .unwrap_or_else(|_| unreachable!("1 is a valid schema version")),
            },
        });
    }

    /// Writes a change nobody asked for — another device's work, or a teacher grading.
    ///
    /// Without this the log would only ever contain what the simulated devices pushed, and a
    /// device would never receive a change it did not cause. Concurrent writers are the normal
    /// case in a classroom.
    pub fn external_change(&mut self, scope: &ScopeId, entity: &EntityName, rng: &mut Rng) {
        let seq = Seq::new(self.next_seq).unwrap_or_else(|_| unreachable!("seq starts at 1"));
        self.next_seq += 1;

        let entity_id = EntityId::new(format!("row:{}", rng.below(8)))
            .unwrap_or_else(|_| unreachable!("derived id is valid"));
        let row_version = RowVersion::new(seq.get()).unwrap_or_else(|_| unreachable!("seq >= 1"));

        self.rows
            .entry(scope.clone())
            .or_default()
            .insert((entity.clone(), entity_id.clone()), row_version);

        self.push_entry(seq, scope, entity, entity_id, row_version);
    }

    /// The protocol version this server speaks.
    #[must_use]
    pub fn protocol() -> ProtocolVersion {
        ProtocolVersion::new(1).unwrap_or_else(|_| unreachable!("1 is a valid protocol version"))
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

/// A well-formed checksum for fields the simulator does not compute for real.
///
/// Batch checksums are not modelled: `docs/spec.md` §5 describes one over the batch's canonical
/// encoding, which is self-referential while the checksum is itself a field of the batch, and
/// settling that is the server slices' work (CS-14/CS-15). What matters here is that the field is
/// present and well-formed, which is what the client's decoder enforces.
fn hex_placeholder() -> HexString {
    HexString::new("00000000000000000000000000000000")
        .unwrap_or_else(|_| unreachable!("32 zeros is valid hex"))
}
