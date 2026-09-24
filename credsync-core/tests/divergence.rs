//! CS-22: what the client does when its digest disagrees with the server's.
//!
//! # Silent divergence is the failure this defends against
//!
//! `docs/spec.md` §5: a digest mismatch means both sides walked the same log and hold different
//! rows. Nothing has errored. Every request succeeded. The user sees plausible data that is quietly
//! wrong, and nobody reports it until trust is gone.
//!
//! The response is to stop trusting the local copy and rebuild it — not to carry on applying
//! changes onto a base already known to be broken, and not to retry forever if the rebuild does not
//! help either.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_core::{Effect, Engine, OutboxEntry, ScopeHealth, Telemetry};
use credsync_protocol::ScopeId;

/// A digest that cannot be what the client computed, forcing a mismatch.
fn wrong_digest() -> credsync_protocol::HexString {
    common::hex("ffffffffffffffffffffffffffffffff")
}

fn other_scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2025-cohort").expect("valid scope")
}

/// Drains effects, returning them.
fn effects<C, E, S, T, Z>(engine: &mut Engine<C, E, S, T, Z>) -> Vec<Effect>
where
    C: credsync_core::Clock,
    E: credsync_core::Entropy,
    S: credsync_core::Storage,
    T: credsync_core::Transport,
    Z: credsync_core::Compressor,
{
    let mut out = Vec::new();
    while let Some(e) = engine.next_effect() {
        out.push(e);
    }
    out
}

/// A bootstrap page, for driving the rebuild path.
fn bootstrap_page(
    seq: u64,
    digest: credsync_protocol::HexString,
    has_more: bool,
) -> credsync_protocol::BootstrapResponse {
    credsync_protocol::BootstrapResponse {
        protocol: common::protocol(),
        scope: common::scope(),
        changes: vec![common::upsert(seq, &format!("r{seq}"), seq)],
        next_cursor: credsync_protocol::Cursor::new(seq).expect("valid cursor"),
        has_more,
        checksum: common::hex("00000000000000000000000000000000"),
        digest,
    }
}

/// The digest a client holds after exactly one row.
fn agreed_digest(_seq: u64, entity_id: &str, row_version: u64) -> credsync_protocol::HexString {
    let mut d = credsync_protocol::ScopeDigest::EMPTY;
    d.add(
        &common::entity(),
        &common::id(entity_id),
        credsync_protocol::RowVersion::new(row_version).expect("valid row version"),
    );
    d.to_hex()
}

/// Drains and discards pending effects.
fn drain<C, E, S, T, Z>(engine: &mut Engine<C, E, S, T, Z>)
where
    C: credsync_core::Clock,
    E: credsync_core::Entropy,
    S: credsync_core::Storage,
    T: credsync_core::Transport,
    Z: credsync_core::Compressor,
{
    while engine.next_effect().is_some() {}
}

// -------------------------------------------------------------------------------------------
// DoD: telemetry carries BOTH digests
// -------------------------------------------------------------------------------------------

/// The divergence report carries the client's digest and the server's.
///
/// One digest says "these disagree" and leaves an engineer with nowhere to go. Both, plus the
/// scope, is enough to ask which rows differ — which is the whole point of reporting it rather
/// than silently healing.
#[test]
fn the_divergence_report_carries_both_digests() {
    let (mut engine, _storage) = common::new_engine();

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");

    let report = effects(&mut engine)
        .into_iter()
        .find_map(|e| match e {
            Effect::Emit(Telemetry::ScopeDiverged {
                scope,
                client,
                server,
            }) => Some((scope, client, server)),
            _ => None,
        })
        .expect("no divergence was reported");

    assert_eq!(report.0, common::scope());
    assert_eq!(
        report.2,
        wrong_digest(),
        "the server's digest was not carried through"
    );
    assert_ne!(
        report.1, report.2,
        "the two digests are identical, so nothing actually diverged"
    );
    assert!(
        !report.1.as_str().is_empty(),
        "the client's own digest is missing, leaving nothing to compare against"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: the scope is tainted and rebuilt
// -------------------------------------------------------------------------------------------

#[test]
fn a_diverged_scope_is_tainted_and_asks_to_be_rebuilt() {
    let (mut engine, _storage) = common::new_engine();
    assert!(engine.scope_health(&common::scope()).is_healthy());

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");

    assert!(
        engine.scope_health(&common::scope()).needs_rebootstrap(),
        "a diverged scope was left looking healthy"
    );
    assert_eq!(
        engine.scopes_needing_rebootstrap(),
        vec![common::scope()],
        "the sync loop has no way to find the scope that needs rebuilding"
    );
}

/// The rebuild clears the scope's rows, cursor and digest — and nothing else.
///
/// Clearing the rows is required rather than tidy: a fresh bootstrap carries no tombstones
/// (`docs/spec.md` §3.1), so a row the client holds that the server no longer has would survive
/// the rebuild and keep the digest wrong forever.
#[test]
fn a_rebuild_clears_the_scope_and_leaves_the_outbox_alone() {
    let (mut engine, storage) = common::new_engine();

    engine
        .apply_batch(&common::batch(vec![
            common::upsert(1, "r1", 1),
            common::upsert(2, "r2", 1),
        ]))
        .expect("applies");
    engine
        .enqueue(OutboxEntry::new(common::command(1, 0), common::schema()))
        .expect("queues");

    assert_eq!(storage.with(|s| s.row_count()), 2);

    engine
        .begin_rebootstrap(&common::scope())
        .expect("rebuilds");

    assert_eq!(
        storage.with(|s| s.row_count()),
        0,
        "the scope's rows survived the rebuild and will keep the digest wrong"
    );
    // Reset to the start, not absent: the rebuild writes `Cursor::START` in the same transaction
    // that clears the rows, so a process killed immediately afterwards finds a scope that is
    // consistently empty rather than one with no cursor at all.
    assert_eq!(
        storage.with(|s| s.cursor(&common::scope())),
        Some(credsync_protocol::Cursor::START),
        "the cursor was not reset, so the rebuild would resume mid-log"
    );
    assert_eq!(
        storage.with(|s| s.queued.len()),
        1,
        "the outbox was cleared to fix a read-side problem — the cure did more damage than the disease"
    );
}

/// A completed rebuild that agrees clears the taint.
#[test]
fn a_rebuild_that_agrees_clears_the_taint() {
    let (mut engine, _storage) = common::new_engine();

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");
    assert!(engine.scope_health(&common::scope()).needs_rebootstrap());

    engine
        .begin_rebootstrap(&common::scope())
        .expect("rebuilds");

    // The rebuild's final page, whose digest matches what the client now computes.
    let change = common::upsert(1, "r1", 1);
    let agreed = {
        let mut d = credsync_protocol::ScopeDigest::EMPTY;
        d.add(&common::entity(), &common::id("r1"), change.row_version);
        d.to_hex()
    };
    engine
        .apply_batch(&common::batch_with_digest(vec![change], agreed))
        .expect("applies");

    assert!(
        engine.scope_health(&common::scope()).is_healthy(),
        "the scope was rebuilt and agrees, but is still marked tainted"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: a tainted scope does not block other scopes
// -------------------------------------------------------------------------------------------

/// One scope diverging leaves the others alone.
///
/// `docs/spec.md` §5 says so directly. A device usually holds several scopes — a cohort, a
/// timetable, a profile — and one broken scope taking the rest offline turns a contained fault
/// into an outage.
#[test]
fn a_tainted_scope_does_not_taint_the_others() {
    let (mut engine, _storage) = common::new_engine_unregistered();
    for scope in [common::scope(), other_scope()] {
        engine
            .registry_mut()
            .register_entity(credsync_protocol::EntityRegistration {
                entity: if scope == common::scope() {
                    common::entity()
                } else {
                    credsync_protocol::EntityName::new("timetables").expect("valid entity")
                },
                scope,
                conflict_class: credsync_protocol::ConflictClass::OwnerDraft,
                schema_version: common::schema(),
            });
    }

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");

    assert!(engine.scope_health(&common::scope()).needs_rebootstrap());
    assert!(
        engine.scope_health(&other_scope()).is_healthy(),
        "one scope diverging marked another as broken"
    );
    assert_eq!(
        engine.scopes_needing_rebootstrap().len(),
        1,
        "the rebuild list spread beyond the scope that actually diverged"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: repeated divergence escalates rather than looping
// -------------------------------------------------------------------------------------------

/// Rebuilds that keep failing stop the healing loop and raise a distinct signal.
///
/// Each round is a genuine **failed heal**: the scope is rebuilt and the rebuild's final page still
/// disagrees. That is what the escalation counts — not the sync loop noticing the same unrepaired
/// fault on successive pulls, which is one divergence however many times it is observed.
///
/// A scope that diverges again immediately after being rebuilt from the server's own snapshot is
/// not suffering a transient fault. Looping would hide that behind a device re-downloading the same
/// scope for ever, on a data budget the user is paying for.
#[test]
fn repeated_failed_rebuilds_escalate_instead_of_looping() {
    let (mut engine, _storage) = common::new_engine();

    for seq in 1..=3u64 {
        engine.begin_rebootstrap(&common::scope()).ok();
        engine
            .apply_bootstrap(&bootstrap_page(seq, wrong_digest(), false))
            .expect("applies");
    }

    let health = engine.scope_health(&common::scope());
    assert!(
        matches!(health, ScopeHealth::Unhealable { .. }),
        "after three failed rebuilds the scope is still being healed: {health:?}"
    );
    assert!(
        !health.needs_rebootstrap(),
        "an unhealable scope is still offered for rebuild, which is the loop"
    );
    assert!(
        engine.scopes_needing_rebootstrap().is_empty(),
        "the sync loop would keep rebuilding a scope that rebuilding does not fix"
    );

    let escalated = effects(&mut engine).into_iter().any(|e| {
        matches!(
            e,
            Effect::Emit(Telemetry::ScopeUnhealable { ref scope, .. }) if *scope == common::scope()
        )
    });
    assert!(
        escalated,
        "healing gave up without telling anyone, which is the silent rot this slice prevents"
    );
}

// -------------------------------------------------------------------------------------------
// The false-divergence bug this slice found
// -------------------------------------------------------------------------------------------

/// A client mid-way through a backlog is not diverged.
///
/// The server's `digest` covers the scope **as it stands now**, not as it stood at the batch's
/// `next_cursor`. A client eight rows into a twenty-row backlog therefore computes a digest over
/// eight rows and disagrees — correctly, and for a reason that has nothing to do with divergence.
///
/// Before CS-22 comparing mid-walk produced noisy telemetry. With self-healing it is far worse:
/// each false mismatch taints the scope, triggers a re-bootstrap, and escalates a perfectly healthy
/// scope to `Unhealable` — a device re-downloading a scope it never had a problem with, on a data
/// budget it is paying for.
///
/// Found by the simulator doing an ordinary catch-up, not by a test written for it.
#[test]
fn a_batch_with_more_to_come_is_never_judged_diverged() {
    let (mut engine, _storage) = common::new_engine();

    let mut batch = common::batch_with_digest(vec![common::upsert(1, "r1", 1)], wrong_digest());
    batch.has_more = true;

    let applied = engine.apply_batch(&batch).expect("applies");

    assert!(
        !applied.diverged,
        "a client part-way through a backlog was reported as diverged"
    );
    assert!(
        engine.scope_health(&common::scope()).is_healthy(),
        "an ordinary catch-up tainted the scope"
    );
    assert!(
        effects(&mut engine)
            .into_iter()
            .all(|e| !matches!(e, Effect::Emit(Telemetry::ScopeDiverged { .. }))),
        "a divergence was reported for a client that is simply behind"
    );
}

/// Walking a long backlog never escalates a healthy scope.
///
/// The consequence of the bug above, at the scale it would actually have appeared: the
/// three-week-offline device that this project's headline case describes.
#[test]
fn a_long_catch_up_does_not_escalate_a_healthy_scope() {
    let (mut engine, _storage) = common::new_engine();

    for seq in 1..=10u64 {
        let mut batch = common::batch_with_digest(
            vec![common::upsert(seq, &format!("r{seq}"), seq)],
            wrong_digest(),
        );
        batch.has_more = seq < 10;
        engine.apply_batch(&batch).expect("applies");
    }

    // The final batch genuinely disagrees, so exactly one divergence is recorded — not ten.
    assert_eq!(
        engine.scope_health(&common::scope()).attempts(),
        1,
        "a single catch-up counted as several divergences and burned the escalation budget"
    );
}

// -------------------------------------------------------------------------------------------
// One divergence per trust episode
// -------------------------------------------------------------------------------------------

/// Pulling a tainted scope again does not count as a new divergence.
///
/// A tainted scope disagrees on **every** subsequent pull: the incremental digest is still wrong
/// and stays wrong until it is rebuilt. Counting each of those reached the escalation threshold
/// after three *pulls* and zero rebuilds — which is not what `Unhealable` means, and which the sync
/// loop reaches simply by doing its job between the taint and the rebuild.
///
/// Found by review on #70.
#[test]
fn repeated_pulls_of_a_tainted_scope_count_as_one_divergence() {
    let (mut engine, _storage) = common::new_engine();

    for seq in 1..=4u64 {
        engine
            .apply_batch(&common::batch_with_digest(
                vec![common::upsert(seq, &format!("r{seq}"), seq)],
                wrong_digest(),
            ))
            .expect("applies");
    }

    assert_eq!(
        engine.scope_health(&common::scope()).attempts(),
        1,
        "four pulls of one broken scope counted as four divergences, so the escalation budget is \
         spent by the sync loop rather than by failed rebuilds"
    );
    assert!(
        engine.scope_health(&common::scope()).needs_rebootstrap(),
        "the scope stopped asking to be rebuilt without ever being rebuilt"
    );
}

/// `ScopeUnhealable` is raised once, not on every subsequent pull.
///
/// It is the event that should reach a person. Re-emitting it on each pull would flood the channel
/// it exists to be noticed in.
#[test]
fn escalation_is_reported_once_not_on_every_pull() {
    let (mut engine, _storage) = common::new_engine();

    // Three failed heals: each divergence is on the final page of a rebuild.
    for seq in 1..=3u64 {
        engine.begin_rebootstrap(&common::scope()).ok();
        engine
            .apply_bootstrap(&bootstrap_page(seq, wrong_digest(), false))
            .expect("applies");
    }
    assert!(matches!(
        engine.scope_health(&common::scope()),
        ScopeHealth::Unhealable { .. }
    ));
    drain(&mut engine);

    // Further ordinary pulls must stay quiet.
    for seq in 4..=6u64 {
        engine
            .apply_batch(&common::batch_with_digest(
                vec![common::upsert(seq, &format!("r{seq}"), seq)],
                wrong_digest(),
            ))
            .expect("applies");
    }

    let repeats = effects(&mut engine)
        .into_iter()
        .filter(|e| matches!(e, Effect::Emit(Telemetry::ScopeUnhealable { .. })))
        .count();
    assert_eq!(
        repeats, 0,
        "an already-escalated scope raised the alert again on every pull"
    );
}

// -------------------------------------------------------------------------------------------
// A healed scope stays healed across a restart
// -------------------------------------------------------------------------------------------

/// A rebuild that agreed is remembered, so a restart does not rebuild it again.
///
/// Held only in memory, the heal was lost on restart and `restore_scope_health` read the stored
/// count as `Tainted` — so the sync loop cleared and re-downloaded a perfectly healthy scope on
/// every launch, for the life of the install. The same loop the count exists to prevent, moved
/// from the escalation path to the heal path.
///
/// Found by review on #70, which also noted that
/// `a_scope_that_cannot_be_healed_stops_being_rebuilt` could not catch it — that test *depends* on
/// the restart turning the scope back into `Tainted`.
#[test]
fn a_healed_scope_is_still_healed_after_a_restart() {
    let (mut engine, storage) = common::new_engine();

    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");
    assert!(engine.scope_health(&common::scope()).needs_rebootstrap());

    engine
        .begin_rebootstrap(&common::scope())
        .expect("rebuilds");
    engine
        .apply_bootstrap(&bootstrap_page(1, agreed_digest(1, "r1", 1), false))
        .expect("applies");
    assert!(engine.scope_health(&common::scope()).is_healthy());

    // The stored record says healed, so a restart does not re-taint it.
    let (attempts, healed) = storage
        .with(|s| s.divergences.get(&common::scope()).copied())
        .expect("a divergence record");
    assert!(healed, "the heal was never written down");

    let (mut restarted, _) = common::new_engine();
    restarted.restore_scope_health(common::scope(), attempts, healed);

    assert!(
        restarted.scope_health(&common::scope()).is_healthy(),
        "a healed scope came back tainted after a restart"
    );
    assert!(
        restarted.scopes_needing_rebootstrap().is_empty(),
        "a healthy scope would be cleared and re-downloaded on every launch"
    );
    assert_eq!(
        restarted.scope_health(&common::scope()).attempts(),
        attempts,
        "the history was forgotten, so a scope that breaks again starts its count over"
    );
}

// -------------------------------------------------------------------------------------------
// clear_scope_health
// -------------------------------------------------------------------------------------------

/// Clearing forgets the history, durably.
#[test]
fn clearing_scope_health_forgets_the_history() {
    let (mut engine, storage) = common::new_engine();
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");
    assert_eq!(engine.scope_health(&common::scope()).attempts(), 1);

    engine.clear_scope_health(&common::scope()).expect("clears");

    assert!(engine.scope_health(&common::scope()).is_healthy());
    assert_eq!(engine.scope_health(&common::scope()).attempts(), 0);
    assert_eq!(
        storage.with(|s| s.divergences.get(&common::scope()).copied()),
        Some((0, false)),
        "the cleared count was not written down, so a restart would bring the history back"
    );
}

/// A failed write leaves the history exactly as it was.
///
/// Reporting a clear that did not commit would have the host believe a scope had been forgiven
/// while storage still says otherwise — and the next restart would contradict the UI.
#[test]
fn a_failed_clear_leaves_the_history_unchanged() {
    let (mut engine, storage) = common::new_engine();
    engine
        .apply_batch(&common::batch_with_digest(
            vec![common::upsert(1, "r1", 1)],
            wrong_digest(),
        ))
        .expect("applies");
    let before = storage.with(|s| s.divergences.get(&common::scope()).copied());

    storage.with_mut(|s| {
        s.fail_next = Some(credsync_core::StorageError::Transient {
            detail: "disk full".to_owned(),
        });
    });

    engine
        .clear_scope_health(&common::scope())
        .expect_err("the clear must fail when the write fails");

    assert_eq!(
        engine.scope_health(&common::scope()).attempts(),
        1,
        "the history was forgotten in memory despite the write failing"
    );
    assert_eq!(
        storage.with(|s| s.divergences.get(&common::scope()).copied()),
        before,
        "a failed clear changed what storage holds"
    );
}
