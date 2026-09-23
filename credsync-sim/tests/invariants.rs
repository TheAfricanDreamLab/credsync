//! CS-12: every invariant, seen to fail.
//!
//! The rule the `credsync-sim` skill states plainly: **never add an invariant you have not seen
//! fail.** An invariant that has never fired is untested — it may be asserting something trivially
//! true, or nothing at all, and the simulator would go on looking busy while checking nothing.
//!
//! So every claim here is exercised twice: once against a state that breaks it, which must be
//! caught, and once against a healthy run, which must be silent. The first half is what makes the
//! second half mean something.
//!
//! These construct broken states directly rather than planting bugs in the engine. That is
//! deliberate — it pins each checker independently, so a drill that fails at CS-13 (#14) points at
//! the engine rather than leaving open whether the checker ever worked.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::Resolution;
use credsync_protocol::{
    Command, CommandId, CommandName, Cursor, EntityId, EntityName, Payload, RowVersion,
    SchemaVersion, ScopeId,
};
use credsync_sim::fakes::{Db, StoredRow};
use credsync_sim::invariant::Invariants;
use credsync_sim::{FaultRates, Trace, World};
use std::cell::RefCell;
use std::rc::Rc;

fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn command_id(n: u8) -> CommandId {
    let mut b = [0u8; 16];
    b[0] = 0x01;
    b[6] = 0x70;
    b[15] = n;
    CommandId::from_bytes(b).expect("version nibble is 7")
}

fn command(n: u8) -> Command {
    Command {
        id: command_id(n),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope(),
        payload: Payload::new(serde_json::json!({ "body": "x" })).expect("valid payload"),
        client_ts: 0,
        checksum: credsync_protocol::HexString::new("00000000000000000000000000000000")
            .expect("valid hex"),
    }
}

fn schema() -> SchemaVersion {
    SchemaVersion::new(1).expect("valid schema version")
}

fn wrap(db: Db) -> Vec<Rc<RefCell<Db>>> {
    vec![Rc::new(RefCell::new(db))]
}

fn row() -> StoredRow {
    StoredRow {
        snapshot: credsync_protocol::Snapshot::new(serde_json::json!({ "v": 1 }))
            .expect("valid snapshot"),
        row_version: RowVersion::new(1).expect("valid row version"),
        schema_version: schema(),
    }
}

// ---------------------------------------------------------------------------------------------
// Cursor monotonicity
// ---------------------------------------------------------------------------------------------

/// A cursor that moves backwards is caught.
///
/// It would re-deliver changes the device already applied, which the engine refuses — so the
/// scope wedges one round trip at a time, forever, while reporting success.
#[test]
fn a_cursor_moving_backwards_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();

    db.cursors.insert(scope(), Cursor::new(90).unwrap());
    let dbs = wrap(db);
    inv.check_step(1, &dbs, &scope());
    assert!(inv.holds(), "a fresh cursor is not a violation");

    dbs[0]
        .borrow_mut()
        .cursors
        .insert(scope(), Cursor::new(40).unwrap());
    inv.check_step(2, &dbs, &scope());

    assert!(
        !inv.holds(),
        "a cursor regressing from 90 to 40 was not caught"
    );
    assert_eq!(inv.violations()[0].invariant, "cursor-monotonicity");
    assert!(inv.violations()[0].detail.contains("90 -> 40"));
}

#[test]
fn a_cursor_moving_forwards_is_not_a_violation() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.cursors.insert(scope(), Cursor::new(10).unwrap());
    let dbs = wrap(db);

    for c in [10u64, 11, 50, 900] {
        dbs[0]
            .borrow_mut()
            .cursors
            .insert(scope(), Cursor::new(c).unwrap());
        inv.check_step(c as i64, &dbs, &scope());
    }
    assert!(
        inv.holds(),
        "monotonic advance must be silent: {:?}",
        inv.violations()
    );
}

// ---------------------------------------------------------------------------------------------
// No-loss
// ---------------------------------------------------------------------------------------------

/// A command that leaves the outbox with no recorded outcome is caught.
///
/// The claim the whole engine is sold on. A student's reflection that vanishes with no trace and
/// no explanation is the failure this project exists to prevent (D-009).
#[test]
fn a_silently_vanished_command_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.outbox.push((command(1), schema()));
    let dbs = wrap(db);

    inv.check_step(1, &dbs, &scope());
    assert!(inv.holds());

    // Gone from the outbox, and nowhere in the resolved log.
    dbs[0].borrow_mut().outbox.clear();
    inv.check_step(2, &dbs, &scope());

    assert!(!inv.holds(), "a command vanished and nothing noticed");
    assert_eq!(inv.violations()[0].invariant, "no-loss");
    assert!(inv.violations()[0].detail.contains("silently lost"));
}

/// Leaving the outbox *into a recorded outcome* is exactly what should happen.
#[test]
fn a_command_resolved_out_of_the_outbox_is_not_a_loss() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.outbox.push((command(1), schema()));
    let dbs = wrap(db);
    inv.check_step(1, &dbs, &scope());

    {
        let mut db = dbs[0].borrow_mut();
        db.outbox.clear();
        db.resolved
            .push((command_id(1), Resolution::Applied { server_seq: None }));
    }
    inv.check_step(2, &dbs, &scope());

    assert!(
        inv.holds(),
        "a properly resolved command was reported as lost: {:?}",
        inv.violations()
    );
}

/// A command in both places at once is caught.
#[test]
fn a_command_both_queued_and_resolved_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.outbox.push((command(1), schema()));
    db.resolved
        .push((command_id(1), Resolution::Applied { server_seq: None }));

    inv.check_step(1, &wrap(db), &scope());

    assert!(!inv.holds());
    assert_eq!(inv.violations()[0].invariant, "no-loss");
    assert!(
        inv.violations()[0]
            .detail
            .contains("both queued and resolved")
    );
}

// ---------------------------------------------------------------------------------------------
// Durability and idempotency
// ---------------------------------------------------------------------------------------------

/// A resolution that disappears is caught.
///
/// Durability: once the server has answered about a command, that answer exists in every future
/// state. A dead letter that quietly vanished would take the user's only explanation with it.
#[test]
fn a_resolution_that_disappears_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.resolved
        .push((command_id(1), Resolution::Applied { server_seq: None }));
    let dbs = wrap(db);

    inv.check_step(1, &dbs, &scope());
    assert!(inv.holds());

    dbs[0].borrow_mut().resolved.clear();
    inv.check_step(2, &dbs, &scope());

    assert!(
        !inv.holds(),
        "a recorded outcome vanished and nothing noticed"
    );
    assert_eq!(inv.violations()[0].invariant, "durability");
}

/// A verdict that changes between steps is caught.
///
/// Idempotency: a replayed push returns the same results again, and the second copy must be a
/// no-op. A command that was applied and later reads as dead-lettered means the user was told two
/// different things about one piece of work.
#[test]
fn a_verdict_changing_across_steps_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.resolved
        .push((command_id(1), Resolution::Applied { server_seq: None }));
    let dbs = wrap(db);
    inv.check_step(1, &dbs, &scope());

    dbs[0].borrow_mut().resolved = vec![(
        command_id(1),
        Resolution::DeadLettered {
            reason: credsync_protocol::Reason::new("changed its mind").expect("valid reason"),
        },
    )];
    inv.check_step(2, &dbs, &scope());

    assert!(!inv.holds(), "a verdict changed and nothing noticed");
    assert_eq!(inv.violations()[0].invariant, "idempotency");
    assert!(inv.violations()[0].detail.contains("applied"));
}

/// The same command recorded twice with different verdicts is caught.
///
/// This is the CS-8 bug in invariant form — a response naming one command twice resolving it
/// twice. A property test caught it once; this catches it under fault conditions no property test
/// would generate.
#[test]
fn one_command_recorded_twice_with_different_verdicts_is_caught() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.resolved
        .push((command_id(1), Resolution::Applied { server_seq: None }));
    db.resolved.push((command_id(1), Resolution::Superseded));

    inv.check_step(1, &wrap(db), &scope());

    assert!(!inv.holds());
    assert_eq!(inv.violations()[0].invariant, "idempotency");
    assert!(inv.violations()[0].detail.contains("twice"));
}

/// A stable verdict repeated across many steps is silent.
#[test]
fn a_stable_verdict_is_not_a_violation() {
    let mut inv = Invariants::new();
    let mut db = Db::default();
    db.resolved
        .push((command_id(1), Resolution::Applied { server_seq: None }));
    db.rows.insert(
        (
            EntityName::new("reflections").unwrap(),
            EntityId::new("r:1").unwrap(),
        ),
        row(),
    );
    let dbs = wrap(db);

    for step in 0..50 {
        inv.check_step(step, &dbs, &scope());
    }
    assert!(
        inv.holds(),
        "a stable state produced violations: {:?}",
        inv.violations()
    );
}

// ---------------------------------------------------------------------------------------------
// Convergence, against a real run
// ---------------------------------------------------------------------------------------------

/// A hostile run converges: every device ends up holding what the server holds.
///
/// The headline claim. Every fault in the menu fires during this run, and afterwards the world is
/// quietened and everyone given time to finish — a device mid-batch is *supposed* to disagree, so
/// convergence is the one invariant whose meaning requires quiescence.
#[test]
fn a_hostile_run_converges() {
    for seed in 0..6 {
        let mut world = World::new(seed, FaultRates::default(), Trace::counting());
        world.run(1_500);
        assert!(
            world.invariants.holds(),
            "seed {seed} violated an invariant: {:?}",
            world.invariants.violations()
        );
    }
}

/// A quiet run converges too, which isolates the faults from the engine.
///
/// If this ever fails while the hostile version passes, something is wrong with the *harness*
/// rather than the engine — the ordinary path is the one nothing should be able to break.
#[test]
fn a_quiet_run_converges() {
    let mut world = World::new(3, FaultRates::none(), Trace::counting());
    world.run(1_500);
    assert!(
        world.invariants.holds(),
        "a world with no faults at all violated an invariant: {:?}",
        world.invariants.violations()
    );
}

/// Devices agree with each other, not merely with the server.
///
/// A weaker check that passes for the wrong reason is worth guarding against: if every device
/// were compared only against a server digest that happened to be empty, convergence would hold
/// vacuously.
#[test]
fn devices_end_up_holding_the_same_rows() {
    let mut world = World::new(11, FaultRates::default(), Trace::counting());
    world.run(1_500);

    let digests: Vec<Option<String>> = world
        .databases()
        .iter()
        .map(|db| {
            db.borrow()
                .digests
                .get(world.scope())
                .map(|h| h.as_str().to_owned())
        })
        .collect();

    assert!(
        digests.windows(2).all(|w| w[0] == w[1]),
        "devices disagreed with each other: {digests:?}"
    );
    assert!(
        digests[0].is_some(),
        "no device recorded a digest at all — the run did nothing"
    );

    let rows: Vec<usize> = world
        .databases()
        .iter()
        .map(|db| db.borrow().rows.len())
        .collect();
    assert!(
        rows[0] > 0,
        "no rows were ever applied; the run proves nothing"
    );
}
