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
use std::collections::BTreeMap;

/// One entry in the append-only change log.
#[derive(Debug, Clone)]
struct LogEntry {
    seq: Seq,
    change: Change,
}

/// What the server decided about a command, kept for replays.
#[derive(Debug, Clone)]
struct DedupeEntry {
    /// The checksum of the body that produced this outcome, so a mutated replay is refused.
    checksum: HexString,
    result: CommandResult,
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
    rows: BTreeMap<ScopeId, BTreeMap<(EntityName, EntityId), RowVersion>>,
    dedupe: BTreeMap<CommandId, DedupeEntry>,
    next_seq: u64,
    /// Which entity each command name writes, mirroring the client's registry.
    command_targets: BTreeMap<String, (EntityName, ScopeId)>,
    /// Commands seen since the last restart. A cold cache loses nothing durable — the dedupe
    /// table is a table, not a cache — but it does lose any in-memory batching state, which is
    /// what `server_restart` exercises.
    pub warm: bool,
}

impl Server {
    /// A server with an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: Vec::new(),
            by_scope: BTreeMap::new(),
            rows: BTreeMap::new(),
            dedupe: BTreeMap::new(),
            next_seq: 1,
            command_targets: BTreeMap::new(),
            warm: true,
        }
    }

    /// Declares which entity and scope a command name writes.
    pub fn register_command(&mut self, name: &str, entity: EntityName, scope: ScopeId) {
        self.command_targets
            .insert(name.to_owned(), (entity, scope));
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
        let Some(rows) = self.rows.get(scope) else {
            return ScopeDigest::EMPTY;
        };
        ScopeDigest::from_rows(rows.iter().map(|((e, i), v)| (e, i, *v)))
    }

    /// The highest `seq` in the log.
    #[must_use]
    pub const fn head(&self) -> u64 {
        self.next_seq - 1
    }

    /// Answers a pull, one batch per requested scope.
    ///
    /// `limit` caps changes per batch so a device walks forward over several cycles, which is
    /// what makes `has_more` and cursor handling do any work at all.
    #[must_use]
    pub fn pull(&self, request: &PullRequest, limit: usize) -> PullResponse {
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

                let changes: Vec<Change> = indices
                    .into_iter()
                    .flat_map(|ix| ix[start..].iter().take(limit))
                    .map(|&i| self.log[i].change.clone())
                    .collect();

                let has_more = available > limit;

                // `next_cursor` covers exactly what was sent. A server that advanced it past
                // undelivered changes would silently skip them; one that left it short would wedge
                // the scope. Both are client-visible, and the client refuses both.
                let next_cursor = changes
                    .last()
                    .map_or_else(|| self.head_for(&sc.scope, after), |c| c.seq.get());

                Batch {
                    scope: sc.scope.clone(),
                    changes,
                    next_cursor: Cursor::new(next_cursor).unwrap_or(Cursor::START),
                    has_more,
                    checksum: hex_placeholder(),
                    digest: self.digest(&sc.scope).to_hex(),
                }
            })
            .collect();

        PullResponse {
            protocol: request.protocol,
            batches,
        }
    }

    /// Where a scope's cursor may advance to when nothing was sent.
    ///
    /// An empty batch still moves the cursor to the log head, so a device subscribed to a quiet
    /// scope does not re-ask about the same empty range forever.
    fn head_for(&self, scope: &ScopeId, after: u64) -> u64 {
        self.by_scope
            .get(scope)
            .and_then(|ix| ix.last())
            .map_or(after, |&i| self.log[i].seq.get())
            .max(after)
    }

    /// Every row a client at `cursor` should be holding, with the version it should be at.
    ///
    /// The authoritative answer to "what should this device have", which is what makes durable
    /// *effects* checkable rather than only durable verdicts. A device that recorded a command as
    /// applied while its row quietly vanished passes every check that inspects only the resolution
    /// log.
    #[must_use]
    pub fn rows_at(&self, scope: &ScopeId, cursor: u64) -> Vec<(EntityName, EntityId, RowVersion)> {
        let Some(indices) = self.by_scope.get(scope) else {
            return Vec::new();
        };
        let mut latest: BTreeMap<(EntityName, EntityId), RowVersion> = BTreeMap::new();
        for &i in indices {
            let e = &self.log[i];
            if e.seq.get() > cursor {
                break;
            }
            latest.insert(
                (e.change.entity.clone(), e.change.entity_id.clone()),
                e.change.row_version,
            );
        }
        latest.into_iter().map(|((e, i), v)| (e, i, v)).collect()
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
        let indices = self.by_scope.get(scope)?;
        indices.iter().rev().find_map(|&i| {
            let e = &self.log[i];
            (e.seq.get() <= cursor
                && e.change.entity == *entity
                && e.change.entity_id == *entity_id)
                .then_some(e.change.row_version)
        })
    }

    /// Applies a push, deduping replays and recording every outcome.
    #[must_use]
    pub fn push(&mut self, request: &PushRequest, rng: &mut Rng) -> PushResponse {
        let results = request
            .commands
            .iter()
            .map(|c| self.apply_command(c, rng))
            .collect();

        PushResponse {
            protocol: request.protocol,
            results,
        }
    }

    fn apply_command(&mut self, command: &Command, rng: &mut Rng) -> CommandResult {
        let checksum = payload_checksum(&command.payload).unwrap_or_else(|_| hex_placeholder());

        if let Some(prior) = self.dedupe.get(&command.id) {
            if prior.checksum == checksum {
                // A replay of the same body. `docs/spec.md` §1: return the recorded outcome
                // without re-applying.
                return prior.result.clone();
            }
            // Same id, different body. Refused as a distinct invalid request rather than deduped
            // as a success, or the dedupe table becomes a way to launder tampered commands.
            let result = CommandResult {
                id: command.id,
                status: Status::Rejected,
                reason: Some(
                    credsync_protocol::Reason::new(
                        "Command id replayed with a different payload checksum.",
                    )
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
        self.log.push(LogEntry {
            seq,
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
