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

/// Diverging repeatedly stops the healing loop and raises a distinct signal.
///
/// A scope that diverges again immediately after being rebuilt from the server's own snapshot is
/// not suffering a transient fault. Looping would hide that behind a device re-downloading the same
/// scope forever, on a data budget the user is paying for.
#[test]
fn repeated_divergence_escalates_instead_of_looping() {
    let (mut engine, _storage) = common::new_engine();

    for attempt in 1..=Engine::<
        common::FakeClock,
        common::FakeEntropy,
        common::SharedStorage,
        common::FakeTransport,
        common::FakeCompressor,
    >::MAX_HEAL_ATTEMPTS
    {
        engine
            .apply_batch(&common::batch_with_digest(
                vec![common::upsert(u64::from(attempt), "r1", u64::from(attempt))],
                wrong_digest(),
            ))
            .expect("applies");
    }

    let health = engine.scope_health(&common::scope());
    assert!(
        matches!(health, ScopeHealth::Unhealable { .. }),
        "after repeated divergence the scope is still asking to be rebuilt: {health:?}"
    );
    assert!(
        !health.needs_rebootstrap(),
        "an unhealable scope is still being offered for rebuild, which is the loop"
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
