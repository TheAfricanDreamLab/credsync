//! CS-20: schema migrations, and the promise that a queued command is never dropped.
//!
//! # The rule these tests defend
//!
//! `docs/spec.md` §7: *"A command whose schema the server no longer accepts is queued, never
//! dropped."* Dropping is the easy implementation and the one that loses a student's coursework
//! while reporting success, so most of what follows checks that nothing disappears — not that
//! migration produces the right answer, which is the host's business.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_core::migrate::{MigrationError, Migrations};
use credsync_protocol::{EntityName, SchemaVersion};
use proptest::prelude::*;
use serde_json::{Value, json};

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

fn v(n: u16) -> SchemaVersion {
    SchemaVersion::new(n).expect("valid schema version")
}

/// Adds `step_<n>` and bumps a counter, so a composed chain is visible in the output.
///
/// Deliberately order-sensitive: `step_1` then `step_2` produces different JSON from `step_2` then
/// `step_1`. A migration harness that ran steps out of order, or skipped one, produces a document
/// that differs rather than one that merely looks plausible.
fn bump(from: u16) -> Value {
    json!({ "applied": from })
}

fn step_1(v: &Value) -> Result<Value, String> {
    let mut out = v.clone();
    out["v1_to_v2"] = json!(true);
    out["trail"] = json!(format!("{}|1->2", trail(v)));
    Ok(out)
}

fn step_2(v: &Value) -> Result<Value, String> {
    let mut out = v.clone();
    out["v2_to_v3"] = json!(true);
    out["trail"] = json!(format!("{}|2->3", trail(v)));
    Ok(out)
}

fn step_3(v: &Value) -> Result<Value, String> {
    let mut out = v.clone();
    out["trail"] = json!(format!("{}|3->4", trail(v)));
    Ok(out)
}

fn always_fails(_: &Value) -> Result<Value, String> {
    Err("this migration cannot run".to_owned())
}

fn trail(v: &Value) -> String {
    v.get("trail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn chain() -> Migrations {
    let mut m = Migrations::new();
    m.register(entity(), v(1), step_1);
    m.register(entity(), v(2), step_2);
    m.register(entity(), v(3), step_3);
    m
}

// -------------------------------------------------------------------------------------------
// DoD: migration composition is associative
// -------------------------------------------------------------------------------------------

/// `v1 -> v2 -> v3` equals `v1 -> v3`, for any starting document and any split point.
///
/// `docs/spec.md` §7 states this directly. It is worth a property test rather than an example
/// because the ways to break it are all about *arithmetic on version numbers* — an off-by-one in
/// the loop bound skips the last step or runs one twice, and a single hand-written case can easily
/// miss both.
#[test]
fn migration_composition_is_associative() {
    let m = chain();

    proptest!(|(start in 1u16..=4, split in 1u16..=4, end in 1u16..=4)| {
        // Only forward, and only through a registered chain.
        prop_assume!(start <= split && split <= end);

        let doc = bump(start);

        let direct = m.migrate_value(&entity(), &doc, v(start), v(end)).expect("direct");
        let first = m.migrate_value(&entity(), &doc, v(start), v(split)).expect("first half");
        let composed = m.migrate_value(&entity(), &first, v(split), v(end)).expect("second half");

        prop_assert_eq!(
            direct,
            composed,
            "v{}->v{} differs from v{}->v{}->v{}",
            start, end, start, split, end
        );
    });
}

/// A no-op migration returns the document untouched.
///
/// The common case by far — every row already at the current version goes through here — so it
/// must not clone-and-rebuild its way to the same answer.
#[test]
fn migrating_to_the_same_version_changes_nothing() {
    let m = chain();
    let doc = json!({ "body": "unchanged", "n": 7 });
    let out = m.migrate_value(&entity(), &doc, v(2), v(2)).expect("no-op");
    assert_eq!(out, doc);
}

/// Every step runs, exactly once, in order.
///
/// The trail makes a skipped or repeated step visible. Without it, a chain that ran `1->2` twice
/// would produce a document that still looks migrated.
#[test]
fn every_step_runs_exactly_once_and_in_order() {
    let m = chain();
    let out = m
        .migrate_value(&entity(), &bump(1), v(1), v(4))
        .expect("full chain");
    assert_eq!(trail(&out), "|1->2|2->3|3->4");
}

// -------------------------------------------------------------------------------------------
// Refusals
// -------------------------------------------------------------------------------------------

/// A backward migration is refused, never attempted.
///
/// A best-effort downgrade discards the fields the older app has no home for, which is data loss
/// that reports success. Refusing is the honest answer.
#[test]
fn a_backward_migration_is_refused() {
    let m = chain();
    let err = m
        .migrate_value(&entity(), &bump(3), v(3), v(1))
        .expect_err("backward");
    assert!(
        matches!(err, MigrationError::Backward { from: 3, to: 1 }),
        "{err:?}"
    );
}

/// A missing step names the step, not just the endpoints.
///
/// "No path from 1 to 4" sends an operator hunting through four registrations; "no step from 2"
/// names the one that is absent.
#[test]
fn a_missing_step_names_the_step_that_is_missing() {
    let mut m = Migrations::new();
    m.register(entity(), v(1), step_1);
    // v2 -> v3 deliberately absent.
    m.register(entity(), v(3), step_3);

    let err = m
        .migrate_value(&entity(), &bump(1), v(1), v(4))
        .expect_err("gap");
    match err {
        MigrationError::NoPath {
            stuck_at, target, ..
        } => {
            assert_eq!(stuck_at, 2, "the gap is at 2, not {stuck_at}");
            assert_eq!(target, 4);
        }
        other => panic!("expected NoPath, got {other:?}"),
    }
}

/// A failing step reports which one failed and why, and produces nothing.
///
/// The caller quarantines the **original**, so a half-migrated value must never escape.
#[test]
fn a_failing_step_reports_it_and_yields_nothing() {
    let mut m = Migrations::new();
    m.register(entity(), v(1), step_1);
    m.register(entity(), v(2), always_fails);

    let err = m
        .migrate_value(&entity(), &bump(1), v(1), v(3))
        .expect_err("failing step");
    match err {
        MigrationError::Failed {
            from, to, reason, ..
        } => {
            assert_eq!((from, to), (2, 3));
            assert_eq!(reason, "this migration cannot run");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// `plan` answers without running anything.
#[test]
fn plan_detects_a_gap_without_running_the_migrations() {
    let mut m = Migrations::new();
    m.register(entity(), v(1), always_fails);

    // The chain is complete for 1 -> 2, so planning succeeds even though running would fail.
    m.plan(&entity(), v(1), v(2)).expect("a complete chain");

    let err = m.plan(&entity(), v(1), v(3)).expect_err("incomplete chain");
    assert!(
        matches!(err, MigrationError::NoPath { stuck_at: 2, .. }),
        "{err:?}"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: no queued command is ever dropped
// -------------------------------------------------------------------------------------------

proptest! {
    /// **Whatever happens, every queued command is still accounted for.**
    ///
    /// The property the slice exists to guarantee. Commands are queued under a mix of schema
    /// versions, against a registry whose migrations are deliberately incomplete, and then a push
    /// is built. Every command must either be *in the request* or *still in the outbox* — never
    /// missing from both.
    ///
    /// `build_push` removes nothing from the outbox by construction, so this also guards against
    /// a future change that decides to "clean up" entries it cannot send.
    #[test]
    fn no_queued_command_is_ever_dropped(
        versions in proptest::collection::vec(1u16..=4, 1..12),
        highest in 1u16..=4,
    ) {
        let (mut engine, _storage) = common::engine_with_schema(highest);

        // A deliberately holey chain: 1->2 and 3->4 exist, 2->3 does not. Anything authored at
        // v1 or v2 heading past v2 is unmigratable, which is exactly the case under test.
        engine.migrations_mut().register(entity(), v(1), step_1);
        engine.migrations_mut().register(entity(), v(3), step_3);

        let mut queued = Vec::new();
        for (n, authored) in versions.iter().enumerate() {
            let command = common::command_n(u8::try_from(n % 250).unwrap_or(0));
            queued.push(command.id);
            engine
                .enqueue(credsync_core::OutboxEntry::new(command, v(*authored)))
                .expect("queues");
        }

        let request = engine
            .build_push(common::protocol(), usize::MAX)
            .expect("builds");
        let sent: std::collections::BTreeSet<_> = request
            .map(|r| r.commands.iter().map(|c| c.id).collect())
            .unwrap_or_default();

        for id in queued {
            prop_assert!(
                sent.contains(&id) || engine.outbox_contains(id),
                "command {} is neither in the push nor in the outbox — it was dropped",
                id
            );
        }
    }
}

/// A command that cannot be migrated is held, and the host is told why.
///
/// Silence would be the worst outcome: the user's edit sits there forever and nothing on screen
/// explains it.
#[test]
fn an_unmigratable_command_is_held_and_reported() {
    let (mut engine, _storage) = common::engine_with_schema(3);
    // 1 -> 2 exists; 2 -> 3 does not, so a v1 command cannot reach v3.
    engine.migrations_mut().register(entity(), v(1), step_1);

    let command = common::command_n(1);
    let id = command.id;
    engine
        .enqueue(credsync_core::OutboxEntry::new(command, v(1)))
        .expect("queues");

    let request = engine
        .build_push(common::protocol(), usize::MAX)
        .expect("builds");

    assert!(
        request.is_none_or(|r| r.commands.is_empty()),
        "an unmigratable command was sent anyway"
    );
    assert!(
        engine.outbox_contains(id),
        "the command left the outbox despite never being sent"
    );

    let mut held = false;
    while let Some(effect) = engine.next_effect() {
        if matches!(
            effect,
            credsync_core::Effect::Emit(credsync_core::Telemetry::CommandHeld { command, .. })
                if command == id
        ) {
            held = true;
        }
    }
    assert!(held, "the host was never told the command is stuck");
}

/// A command authored under an older schema is migrated before it goes on the wire.
#[test]
fn a_queued_command_is_migrated_forward_before_push() {
    let (mut engine, _storage) = common::engine_with_schema(3);
    engine.migrations_mut().register(entity(), v(1), step_1);
    engine.migrations_mut().register(entity(), v(2), step_2);

    engine
        .enqueue(credsync_core::OutboxEntry::new(common::command_n(1), v(1)))
        .expect("queues");

    let request = engine
        .build_push(common::protocol(), usize::MAX)
        .expect("builds")
        .expect("a request");

    let sent = request.commands.first().expect("one command");
    assert_eq!(
        trail(sent.payload.as_value()),
        "|1->2|2->3",
        "the command went out without being migrated"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: a migration failure quarantines the row rather than corrupting it
// -------------------------------------------------------------------------------------------

/// A row that cannot be migrated is set aside with its original bytes, and surfaced.
///
/// Three things have to hold at once, and dropping any one of them is a plausible implementation:
///
/// 1. The row is **not** written into the live table, because the app cannot read it.
/// 2. The **original** snapshot is kept, not a half-migrated one — a later app version that knows
///    the migration is the only reason to keep it at all.
/// 3. The host is told, because the user will experience this as something missing.
#[test]
fn a_row_that_cannot_be_migrated_is_quarantined_with_its_original_bytes() {
    let (mut engine, storage) = common::engine_with_schema(3);
    // 1 -> 2 exists; 2 -> 3 does not, so a v1 row cannot reach v3.
    engine.migrations_mut().register(entity(), v(1), step_1);

    let mut change = common::upsert(1, "r1", 1);
    change.schema_version = v(1);
    let original = change.snapshot.clone().expect("an upsert has a snapshot");

    engine
        .apply_batch(&common::batch(vec![change]))
        .expect("applies");

    assert_eq!(
        storage.with(|s| s.row_count()),
        0,
        "an unreadable row was written into the live table"
    );

    let quarantined = storage.with(|s| s.quarantined());
    assert_eq!(quarantined.len(), 1, "the row was not quarantined");
    assert_eq!(
        quarantined[0].1, original,
        "the quarantined snapshot is not the one that arrived"
    );
    assert_eq!(
        quarantined[0].2,
        v(1),
        "the quarantined row lost the schema version it was written under"
    );

    let mut reported = false;
    while let Some(effect) = engine.next_effect() {
        if matches!(
            effect,
            credsync_core::Effect::Emit(credsync_core::Telemetry::RowQuarantined { .. })
        ) {
            reported = true;
        }
    }
    assert!(reported, "the host was never told a row was quarantined");
}

/// A quarantined row still counts toward the scope digest.
///
/// The device *received* it; it simply cannot read it. Leaving it out would report divergence
/// against a server the client has not diverged from — and send it re-bootstrapping straight back
/// into the same unreadable row, forever.
#[test]
fn a_quarantined_row_does_not_cause_false_divergence() {
    let (mut engine, _storage) = common::engine_with_schema(3);
    engine.migrations_mut().register(entity(), v(1), step_1);

    // A batch whose digest is what the server computes for one row at version 1.
    let mut change = common::upsert(1, "r1", 1);
    change.schema_version = v(1);

    let server_digest = {
        let mut d = credsync_protocol::ScopeDigest::EMPTY;
        d.add(&entity(), &common::id("r1"), change.row_version);
        d.to_hex()
    };

    let applied = engine
        .apply_batch(&common::batch_with_digest(vec![change], server_digest))
        .expect("applies");

    assert!(
        !applied.diverged,
        "quarantining a row reported divergence the client has not actually suffered"
    );
}

/// A row already at this app's version is untouched, whatever is registered.
#[test]
fn a_row_at_the_current_version_is_not_migrated() {
    let (mut engine, storage) = common::engine_with_schema(1);
    // A step that would corrupt anything it touched. It must never run.
    engine
        .migrations_mut()
        .register(entity(), v(1), always_fails);

    let mut change = common::upsert(1, "r1", 1);
    change.schema_version = v(1);
    let original = change.snapshot.clone().expect("an upsert has a snapshot");

    engine
        .apply_batch(&common::batch(vec![change]))
        .expect("applies");

    assert_eq!(
        storage.with(|s| s.row_count()),
        1,
        "the row was not applied"
    );
    assert_eq!(
        storage.with(|s| s
            .row(&entity(), &common::id("r1"))
            .map(|r| r.snapshot.clone())),
        Some(original),
        "a row at the current version was altered"
    );
}
