//! The claims credSync makes, checked continuously.
//!
//! An invariant is a statement that must hold at **every step**, not merely once the run has gone
//! quiet. Design §7.1 is explicit about that, and the reason is worth keeping in view: a bug that
//! self-corrects before the run ends is still a bug. It corrupted state, and the only thing that
//! saved the user was a later event happening to paper over it. Check only at quiescence and the
//! entire class is invisible.
//!
//! # These are the product
//!
//! Everything else in `credsync-sim` — the generator, the fault menu, the fake traits — exists so
//! that these can be evaluated against a device life nobody could produce by hand. Until CS-12
//! the simulator could only report that it ran. From here a seed can *fail*, and that is the
//! whole point.
//!
//! # Never add one you have not seen fail
//!
//! An invariant that has never fired is untested: it may be asserting something trivially true,
//! or nothing at all. Break the code deliberately, watch it catch the break, then revert. CS-13
//! (#14) is that drill run in earnest against three planted bugs.

use crate::fakes::Db;
use crate::server::Server;
use credsync_core::Resolution;
use credsync_protocol::{CommandId, Cursor, ScopeDigest, ScopeId};
use std::collections::{BTreeMap, BTreeSet};

/// An invariant that did not hold.
///
/// Carries enough to start diagnosis without re-running: which claim broke, which device, and
/// what the two sides actually held. The seed is added by the caller, because the seed is the
/// reproduction and belongs at the top of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Which claim failed.
    pub invariant: &'static str,
    /// The device it failed on, or `None` for a claim about the world.
    pub device: Option<usize>,
    /// What went wrong, in a sentence someone can act on.
    pub detail: String,
    /// Simulated time at the moment of failure.
    pub at_ms: i64,
}

impl core::fmt::Display for Violation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.device {
            Some(d) => write!(
                f,
                "[{}] device {d} at {}ms: {}",
                self.invariant, self.at_ms, self.detail
            ),
            None => write!(
                f,
                "[{}] at {}ms: {}",
                self.invariant, self.at_ms, self.detail
            ),
        }
    }
}

/// What the checker remembers between steps.
///
/// Several of these claims are about *change over time* — a cursor never moving backwards, an
/// acknowledged command never becoming unacknowledged — and cannot be evaluated from a single
/// snapshot. This is that history, kept deliberately small: one number or one set per device.
#[derive(Debug, Default)]
pub struct Invariants {
    /// The highest cursor each device has ever reached, per scope.
    high_water: BTreeMap<(usize, ScopeId), u64>,
    /// Commands each device has seen resolved, and how.
    ///
    /// Durability and idempotency are both claims about this set never losing a member and never
    /// changing a member's verdict.
    resolved: BTreeMap<usize, BTreeMap<CommandId, Verdict>>,
    /// Commands each device has ever had queued, so a disappearance can be noticed.
    ever_queued: BTreeMap<usize, BTreeSet<CommandId>>,
    /// Violations found so far.
    violations: Vec<Violation>,
}

/// A resolution reduced to what the invariants care about.
///
/// Comparing `Resolution` values directly would make a changed *reason string* look like a
/// changed verdict, which is not what durability claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Applied,
    Superseded,
    DeadLettered,
}

impl Verdict {
    const fn of(r: &Resolution) -> Self {
        match r {
            Resolution::Applied { .. } => Self::Applied,
            Resolution::Superseded => Self::Superseded,
            _ => Self::DeadLettered,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Superseded => "superseded",
            Self::DeadLettered => "dead-lettered",
        }
    }
}

impl Invariants {
    /// A fresh checker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything that has failed so far.
    #[must_use]
    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }

    /// Whether every claim still holds.
    #[must_use]
    pub fn holds(&self) -> bool {
        self.violations.is_empty()
    }

    /// Checks every per-step claim against the world as it stands right now.
    ///
    /// Called after **every** step, which is what "continuously" means. The cost is a handful of
    /// map lookups per device; the alternative is not seeing a whole class of bug.
    pub fn check_step(
        &mut self,
        at_ms: i64,
        databases: &[std::rc::Rc<std::cell::RefCell<Db>>],
        scope: &ScopeId,
    ) {
        for (i, db) in databases.iter().enumerate() {
            let db = db.borrow();
            self.check_cursor_monotonic(at_ms, i, &db, scope);
            self.check_durability_and_idempotency(at_ms, i, &db);
            self.check_no_loss(at_ms, i, &db);
        }
    }

    /// **Cursor monotonicity.** A cursor never moves backwards.
    ///
    /// A cursor that regressed would re-deliver changes the device already applied, and the
    /// engine refuses those — so the scope would wedge, one round trip at a time, forever. The
    /// device would report success and stop receiving anything new.
    fn check_cursor_monotonic(&mut self, at_ms: i64, device: usize, db: &Db, scope: &ScopeId) {
        let Some(cursor) = db.cursors.get(scope) else {
            // A cursor that *disappears* is a regression to nothing, and the worst kind: the
            // device would re-walk the log from the beginning, re-applying everything. Skipping
            // the check here would let a cursor reset pass as success.
            if self.high_water.contains_key(&(device, scope.clone())) {
                let was = self.high_water[&(device, scope.clone())];
                self.violations.push(Violation {
                    invariant: "cursor-monotonicity",
                    device: Some(device),
                    detail: format!("cursor reached {was} and then vanished from storage"),
                    at_ms,
                });
                self.high_water.remove(&(device, scope.clone()));
            }
            return;
        };
        let now = cursor.get();
        let key = (device, scope.clone());
        let high = self.high_water.entry(key).or_insert(0);

        if now < *high {
            let was = *high;
            self.violations.push(Violation {
                invariant: "cursor-monotonicity",
                device: Some(device),
                detail: format!("cursor moved backwards, {was} -> {now}"),
                at_ms,
            });
        }
        *high = (*high).max(now);
    }

    /// **Durability** and **idempotency**, which are two readings of one rule.
    ///
    /// Durability: once a command is resolved, that resolution exists in every future state — it
    /// never vanishes. Idempotency: applying the same result again changes nothing, so a verdict
    /// never *changes* either.
    ///
    /// Both matter because the network duplicates and the process dies. A device that re-pushed
    /// after a lost acknowledgement gets the same answer back from the server's dedupe table, and
    /// the second copy must be a no-op rather than a second, possibly different, outcome.
    fn check_durability_and_idempotency(&mut self, at_ms: i64, device: usize, db: &Db) {
        let seen = self.resolved.entry(device).or_default();

        // A duplicate inside the stored log itself would mean the engine recorded one command
        // twice — the CS-8 bug, which a property test caught once and this would catch again
        // under fault conditions no property test generates.
        let mut this_run: BTreeMap<CommandId, Verdict> = BTreeMap::new();
        for (id, resolution) in &db.resolved {
            let verdict = Verdict::of(resolution);
            if let Some(prior) = this_run.insert(*id, verdict) {
                // Any repeat, not only a contradictory one. `docs/spec.md` §3.3 gives one result
                // per submitted command, so one command with two recorded outcomes is wrong even
                // when the two agree — a user-facing list of dead letters is built from this
                // count. Limiting the check to differing verdicts made it blind to a duplicated
                // push result, whose two copies are identical by construction.
                let detail = if prior == verdict {
                    format!(
                        "command {id} recorded twice with the same verdict, {}",
                        verdict.name()
                    )
                } else {
                    format!(
                        "command {id} recorded twice with different verdicts, {} then {}",
                        prior.name(),
                        verdict.name()
                    )
                };
                self.violations.push(Violation {
                    invariant: "idempotency",
                    device: Some(device),
                    detail,
                    at_ms,
                });
            }
        }

        for (id, verdict) in &this_run {
            match seen.get(id) {
                None => {
                    seen.insert(*id, *verdict);
                }
                Some(prior) if prior != verdict => {
                    let prior = *prior;
                    self.violations.push(Violation {
                        invariant: "idempotency",
                        device: Some(device),
                        detail: format!(
                            "command {id} changed verdict across steps, {} -> {}",
                            prior.name(),
                            verdict.name()
                        ),
                        at_ms,
                    });
                }
                Some(_) => {}
            }
        }

        // Durability: nothing that was resolved may stop being resolved.
        let lost: Vec<CommandId> = seen
            .keys()
            .filter(|id| !this_run.contains_key(id))
            .copied()
            .collect();
        for id in lost {
            self.violations.push(Violation {
                invariant: "durability",
                device: Some(device),
                detail: format!("command {id} was resolved and is no longer recorded"),
                at_ms,
            });
            // Drop it so the same loss is not reported on every subsequent step; one report per
            // failure, not one per step thereafter.
            self.resolved.entry(device).or_default().remove(&id);
        }
    }

    /// **No-loss.** An outbox entry leaves only into a recorded outcome.
    ///
    /// The claim the whole engine is sold on. Platform Plan v1.1 calls silent loss trust-fatal and
    /// D-009 makes it a protocol violation rather than a tradeoff: a student's reflection that
    /// vanishes with no trace and no explanation is the failure this project exists to prevent.
    ///
    /// Every command is therefore in exactly one of two places at every instant — queued, or
    /// resolved. Never both, never neither.
    fn check_no_loss(&mut self, at_ms: i64, device: usize, db: &Db) {
        let queued: BTreeSet<CommandId> = db.outbox.iter().map(|(c, _)| c.id).collect();
        let resolved: BTreeSet<CommandId> = db.resolved.iter().map(|(id, _)| *id).collect();

        for id in queued.intersection(&resolved) {
            self.violations.push(Violation {
                invariant: "no-loss",
                device: Some(device),
                detail: format!("command {id} is both queued and resolved"),
                at_ms,
            });
        }

        let ever = self.ever_queued.entry(device).or_default();
        for id in &queued {
            ever.insert(*id);
        }

        let vanished: Vec<CommandId> = ever
            .iter()
            .filter(|id| !queued.contains(id) && !resolved.contains(id))
            .copied()
            .collect();
        for id in vanished {
            self.violations.push(Violation {
                invariant: "no-loss",
                device: Some(device),
                detail: format!(
                    "command {id} left the outbox with no recorded outcome — silently lost"
                ),
                at_ms,
            });
            self.ever_queued.entry(device).or_default().remove(&id);
        }
    }

    /// **Convergence.** After quiet, every device's digest equals the server's.
    ///
    /// Checked at the end of a run rather than continuously, and that exception is deliberate: a
    /// device mid-batch is *supposed* to disagree with the server, because it has not finished
    /// applying yet. Convergence is a claim about where things settle, so it is the one invariant
    /// whose meaning requires quiescence.
    ///
    /// A device that never converges is holding rows the server does not have, or missing rows it
    /// does — the silent divergence the digest exists to detect.
    pub fn check_convergence(
        &mut self,
        at_ms: i64,
        databases: &[std::rc::Rc<std::cell::RefCell<Db>>],
        server: &Server,
        scope: &ScopeId,
    ) {
        let expected = server.digest(scope).to_hex();

        let head = server.head();

        for (i, db) in databases.iter().enumerate() {
            let db = db.borrow();

            let Some(actual) = db.digests.get(scope) else {
                // A device with no digest at all has converged to nothing. That is only innocent
                // when there was nothing to receive: if the server has changes, this device has
                // been quietly skipped by the check that exists to notice exactly this.
                if head > 0 {
                    self.violations.push(Violation {
                        invariant: "convergence",
                        device: Some(i),
                        detail: format!(
                            "no digest recorded after settling, while the server holds {head} \
                             changes"
                        ),
                        at_ms,
                    });
                }
                continue;
            };

            // Convergence is a claim about where things settle, so a device still mid-log has not
            // yet had its chance. But "not caught up after settling" is a failure rather than an
            // exemption — `World::settle` runs a fixed number of steps, so a device that is still
            // behind is one the run gave up on, and skipping it silently would let the whole run
            // report success.
            let cursor = db.cursors.get(scope).map_or(0, |c| c.get());
            if cursor < head {
                self.violations.push(Violation {
                    invariant: "convergence",
                    device: Some(i),
                    detail: format!(
                        "still at cursor {cursor} after settling, with the server at {head}; the \
                         device never caught up"
                    ),
                    at_ms,
                });
                continue;
            }

            if *actual != expected {
                self.violations.push(Violation {
                    invariant: "convergence",
                    device: Some(i),
                    detail: format!(
                        "digest {actual} does not match the server's {expected} after catching up"
                    ),
                    at_ms,
                });
            }
        }
    }

    /// **Policy conformance**, per entity class, from the registry declaration.
    ///
    /// The registry says `reflections` is an owner draft, so commands against it are permitted and
    /// a superseded edit must be recoverable. A server-authoritative entity would permit no
    /// commands at all. Generated from the declaration rather than hardcoded, so the claim tracks
    /// whatever the host actually registered (CS-9, D-047).
    pub fn check_policy(
        &mut self,
        at_ms: i64,
        databases: &[std::rc::Rc<std::cell::RefCell<Db>>],
        registry: &credsync_core::Registry,
    ) {
        for (i, db) in databases.iter().enumerate() {
            let db = db.borrow();

            for (id, entity, _) in &db.recovered {
                match registry.class_of(entity) {
                    Some(credsync_protocol::ConflictClass::OwnerDraft) => {}
                    other => {
                        self.violations.push(Violation {
                            invariant: "policy-conformance",
                            device: Some(i),
                            detail: format!(
                                "a draft was recovered for command {id} against entity {entity}, \
                                 whose class is {other:?} — only owner drafts have drafts to \
                                 recover"
                            ),
                            at_ms,
                        });
                    }
                }
            }

            // Every superseded owner-draft command must have left a recoverable draft behind.
            // docs/spec.md §6: the losing version returns to the device and is stored. Silent
            // loss is a protocol violation, not a tradeoff.
            let recovered: BTreeSet<CommandId> =
                db.recovered.iter().map(|(id, _, _)| *id).collect();
            for (id, resolution) in &db.resolved {
                if matches!(resolution, Resolution::Superseded) && !recovered.contains(id) {
                    self.violations.push(Violation {
                        invariant: "policy-conformance",
                        device: Some(i),
                        detail: format!(
                            "command {id} was superseded but no draft was recovered; the user's \
                             edit is gone"
                        ),
                        at_ms,
                    });
                }
            }
        }
    }

    /// **Durable effects.** A row the cursor implies must actually be present.
    ///
    /// The verdict checks above inspect only `db.resolved`, so an applied command whose row
    /// vanished would still pass: the resolution is recorded, and nothing looks at whether the
    /// *effect* survived. That is the difference between remembering that a student's reflection
    /// saved and the reflection still being there.
    ///
    /// So every row the server holds at or below the device's cursor must exist on the device,
    /// at the version the cursor implies. Checked continuously.
    pub fn check_durable_effects(
        &mut self,
        at_ms: i64,
        databases: &[std::rc::Rc<std::cell::RefCell<Db>>],
        server: &Server,
        scope: &ScopeId,
    ) {
        for (i, db) in databases.iter().enumerate() {
            let db = db.borrow();
            let Some(cursor) = db.cursors.get(scope) else {
                continue;
            };
            let cursor = cursor.get();

            for (entity, entity_id, expected) in server.rows_at(scope, cursor) {
                match db.rows.get(&(entity.clone(), entity_id.clone())) {
                    Some(stored) if stored.row_version == expected => {}
                    Some(stored) => {
                        self.violations.push(Violation {
                            invariant: "durable-effects",
                            device: Some(i),
                            detail: format!(
                                "{entity}/{entity_id} is at version {} but the cursor {cursor} \
                                 implies {}",
                                stored.row_version.get(),
                                expected.get()
                            ),
                            at_ms,
                        });
                    }
                    None => {
                        self.violations.push(Violation {
                            invariant: "durable-effects",
                            device: Some(i),
                            detail: format!(
                                "{entity}/{entity_id} should exist at version {} for cursor \
                                 {cursor}, and is absent — an applied effect was lost",
                                expected.get()
                            ),
                            at_ms,
                        });
                    }
                }
            }
        }
    }

    /// A cursor can never exceed the server's log head.
    ///
    /// A device claiming to have applied changes the server has not written would skip everything
    /// between — permanently, and reporting success the whole time.
    pub fn check_cursor_bounds(
        &mut self,
        at_ms: i64,
        databases: &[std::rc::Rc<std::cell::RefCell<Db>>],
        server: &Server,
        scope: &ScopeId,
    ) {
        let head = server.head();
        for (i, db) in databases.iter().enumerate() {
            let db = db.borrow();
            let Some(cursor) = db.cursors.get(scope) else {
                continue;
            };
            if cursor.get() > head {
                self.violations.push(Violation {
                    invariant: "cursor-bounds",
                    device: Some(i),
                    detail: format!(
                        "cursor {} is ahead of the server's log head {head}",
                        cursor.get()
                    ),
                    at_ms,
                });
            }
        }
    }
}

/// The empty digest, for comparisons that need a baseline.
#[must_use]
pub const fn empty_digest() -> ScopeDigest {
    ScopeDigest::EMPTY
}

/// A cursor at the start, for comparisons that need a baseline.
#[must_use]
pub const fn start_cursor() -> Cursor {
    Cursor::START
}
