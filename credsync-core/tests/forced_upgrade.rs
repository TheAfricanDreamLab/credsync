//! CS-21: what a client does when the server says its protocol is too old.
//!
//! # The claim under test
//!
//! `docs/spec.md` §7: *"The client then queues its outbox and surfaces an upgrade prompt — it never
//! drops queued work."*
//!
//! A forced upgrade is the worst moment to lose anything. The user cannot sync, cannot fix it
//! themselves beyond updating the app, and may be days from a connection good enough to do that —
//! so whatever is queued has to survive not just this push but everything that happens until an
//! update lands.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_core::{Effect, OutboxEntry, Telemetry};
use credsync_protocol::{ForcedUpgrade, ProtocolVersion, Reason};
use proptest::prelude::*;

fn protocol_n(n: u16) -> ProtocolVersion {
    ProtocolVersion::new(n).expect("valid protocol version")
}

fn envelope(min: u16, current: u16) -> ForcedUpgrade {
    ForcedUpgrade {
        min_protocol: protocol_n(min),
        current_protocol: protocol_n(current),
        reason: Reason::new("This app is too old to sync. Please update it to continue.")
            .expect("valid reason"),
    }
}

// -------------------------------------------------------------------------------------------
// DoD: the client queues its outbox on forced upgrade
// -------------------------------------------------------------------------------------------

proptest! {
    /// **Nothing queued is lost when the server forces an upgrade.**
    ///
    /// The property the slice exists for. Commands are queued, the server refuses the protocol,
    /// and every one of them must still be in the outbox afterwards — including after further
    /// push attempts, which is where a "clear what we cannot send" implementation would bite.
    #[test]
    fn a_forced_upgrade_never_drops_queued_work(
        count in 1usize..24,
        attempts in 1u8..4,
    ) {
        let (mut engine, storage) = common::new_engine();

        let mut queued = Vec::new();
        for n in 0..count {
            let command = common::command(u8::try_from(n % 250).unwrap_or(0), 0);
            queued.push(command.id);
            engine
                .enqueue(OutboxEntry::new(command, common::schema()))
                .expect("queues");
        }

        engine.on_upgrade_required(&envelope(4, 5));

        // Every push after the refusal must be a no-op, not a cleanup.
        for _ in 0..attempts {
            let request = engine
                .build_push(common::protocol(), usize::MAX)
                .expect("builds");
            prop_assert!(
                request.is_none(),
                "a push was built after the server refused this protocol version"
            );
        }

        for id in &queued {
            prop_assert!(
                engine.outbox_contains(*id),
                "command {} left the outbox during a forced upgrade",
                id
            );
        }

        // And durably, not merely in memory: the app is about to be killed and updated.
        prop_assert_eq!(
            storage.with(|s| s.queued.len()),
            queued.len(),
            "the durable outbox lost entries the engine still believes it holds"
        );
    }
}

/// The upgrade prompt carries what is at stake.
///
/// "Please update to continue" is easy to dismiss. "Please update — 7 edits are waiting" is not,
/// and the difference matters because the queued work is safe only while the app stays installed.
#[test]
fn the_upgrade_prompt_reports_how_much_is_waiting() {
    let (mut engine, _storage) = common::new_engine();
    for n in 0..7u8 {
        engine
            .enqueue(OutboxEntry::new(common::command(n, 0), common::schema()))
            .expect("queues");
    }

    engine.on_upgrade_required(&envelope(4, 5));

    let mut reported = None;
    while let Some(effect) = engine.next_effect() {
        if let Effect::Emit(Telemetry::UpgradeRequired {
            queued,
            min_protocol,
            current_protocol,
            ..
        }) = effect
        {
            reported = Some((queued, min_protocol, current_protocol));
        }
    }

    let (queued, min, current) = reported.expect("no upgrade prompt was emitted");
    assert_eq!(queued, 7, "the prompt did not say how much work is waiting");
    assert_eq!(min, protocol_n(4));
    assert_eq!(current, protocol_n(5));
}

/// The refusal is remembered, so a host can ask at any point rather than catching one event.
#[test]
fn the_forced_upgrade_is_readable_after_the_fact() {
    let (mut engine, _storage) = common::new_engine();
    assert!(engine.upgrade_required().is_none());

    engine.on_upgrade_required(&envelope(4, 5));

    let held = engine
        .upgrade_required()
        .expect("the refusal is remembered");
    assert_eq!(held.min_protocol, protocol_n(4));
}

/// After the app updates, the queued work sends — all of it.
///
/// Queueing is only half the promise. Work held forever is still work lost; it has to go out on the
/// other side of the update.
#[test]
fn queued_work_sends_once_the_upgrade_is_done() {
    let (mut engine, _storage) = common::new_engine();
    let mut queued = Vec::new();
    for n in 0..5u8 {
        let command = common::command(n, 0);
        queued.push(command.id);
        engine
            .enqueue(OutboxEntry::new(command, common::schema()))
            .expect("queues");
    }

    engine.on_upgrade_required(&envelope(4, 5));
    assert!(
        engine
            .build_push(common::protocol(), usize::MAX)
            .expect("builds")
            .is_none()
    );

    engine.upgrade_completed();

    let request = engine
        .build_push(common::protocol(), usize::MAX)
        .expect("builds")
        .expect("a request");
    let sent: std::collections::BTreeSet<_> = request.commands.iter().map(|c| c.id).collect();
    for id in &queued {
        assert!(
            sent.contains(id),
            "command {id} was not sent after the upgrade completed"
        );
    }
}

/// A forced upgrade does not stop the client applying what it is given.
///
/// Reading is still useful to somebody who cannot write. A blank screen plus "update the app" is a
/// worse answer than today's timetable plus "update the app", and the server refuses the request
/// itself if it disagrees.
#[test]
fn a_forced_upgrade_does_not_stop_the_client_applying_batches() {
    let (mut engine, storage) = common::new_engine();
    engine.on_upgrade_required(&envelope(4, 5));

    engine
        .apply_batch(&common::batch(vec![common::upsert(1, "r1", 1)]))
        .expect("applies");

    assert_eq!(
        storage.with(|s| s.row_count()),
        1,
        "the client stopped applying batches during a forced upgrade"
    );
}
