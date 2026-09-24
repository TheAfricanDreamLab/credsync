//! CS-21: an old client against a newer server, end to end.
//!
//! The scenario `docs/spec.md` §7 exists for: a server has moved on, a device has not, and the
//! device is holding work. What must not happen is the work disappearing — not when the server
//! refuses it, not on the pushes that follow, and not while the user waits for an update they may
//! be days away from being able to install.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::{Engine, OutboxEntry};
use credsync_protocol::{
    Command, CommandId, CommandName, ConflictClass, Cursor, EntityName, EntityRegistration,
    HexString, Payload, ProtocolVersion, PullRequest, SchemaVersion, ScopeCursor, ScopeId,
};
use credsync_server::version::{Negotiated, VersionPolicy};
use credsync_sim::fakes::{SimClock, SimCompressor, SimEntropy, SimStorage, SimTransport};
use credsync_sim::{Rng, Server};
use serde_json::json;

type SimEngine = Engine<SimClock, SimEntropy, SimStorage, SimTransport, SimCompressor>;

/// The server has moved to protocol 5, so it accepts 5 and 4.
const SERVER_CURRENT: u16 = 5;
/// The device is two versions behind: outside the window.
const OLD_CLIENT: u16 = 3;
/// A device one version behind: still inside the window.
const RECENT_CLIENT: u16 = 4;

fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

fn p(n: u16) -> ProtocolVersion {
    ProtocolVersion::new(n).expect("valid protocol version")
}

fn device(storage: SimStorage) -> SimEngine {
    let mut engine = Engine::new(
        SimClock::new(1_756_137_600_000, 0, 0),
        SimEntropy::new(1),
        storage,
        SimTransport::new(),
        SimCompressor::with_ratio(3),
    );
    engine.registry_mut().register_entity(EntityRegistration {
        entity: entity(),
        scope: scope(),
        conflict_class: ConflictClass::OwnerDraft,
        schema_version: SchemaVersion::new(1).expect("valid schema version"),
    });
    engine.registry_mut().register_command(
        CommandName::new("submit_reflection").expect("valid name"),
        entity(),
    );
    engine
}

fn command_n(n: u8) -> Command {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[15] = n;
    Command {
        id: CommandId::from_bytes(bytes).expect("version nibble is 7"),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope(),
        payload: Payload::new(json!({ "v": n })).expect("valid payload"),
        client_ts: 1_756_137_600_000,
        checksum: HexString::new("00000000000000000000000000000000").expect("valid hex"),
    }
}

/// Queues `n` commands and returns their ids.
fn queue(engine: &mut SimEngine, n: u8) -> Vec<CommandId> {
    (0..n)
        .map(|i| {
            let command = command_n(i);
            let id = command.id;
            engine
                .enqueue(OutboxEntry::new(
                    command,
                    SchemaVersion::new(1).expect("valid schema version"),
                ))
                .expect("queues");
            id
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// DoD: an old client against a newer server, through the window
// -------------------------------------------------------------------------------------------

/// A device one version behind keeps working.
///
/// The point of the N−1 window: an app update rolls out over days, and the devices that have not
/// taken it yet must not stop dead.
#[test]
fn a_device_one_version_behind_is_still_served() {
    let policy = VersionPolicy::window(p(SERVER_CURRENT));
    assert!(
        policy.negotiate(p(RECENT_CLIENT)).is_accepted(),
        "a device one version behind was cut off mid-rollout"
    );
}

/// A device two versions behind is refused, holds its work, and sends it after the update.
///
/// The whole arc in one test, because the halves are only meaningful together: refusing without
/// keeping the work is data loss, and keeping it without ever sending it is data loss taking
/// longer.
#[test]
fn an_old_device_is_refused_holds_its_work_and_sends_it_after_updating() {
    let policy = VersionPolicy::window(p(SERVER_CURRENT));
    let storage = SimStorage::new();
    let mut engine = device(storage.clone());

    let queued = queue(&mut engine, 6);

    // The device tries to sync and the server refuses its protocol version.
    let Negotiated::Refused(envelope) = policy.negotiate(p(OLD_CLIENT)) else {
        panic!("a device two versions behind was accepted");
    };
    engine.on_upgrade_required(&envelope);

    // Several cycles go by while the user waits for an update.
    for _ in 0..5 {
        assert!(
            engine
                .build_push(p(OLD_CLIENT), 1_000_000)
                .expect("builds")
                .is_none(),
            "the device kept pushing at a version the server had already refused"
        );
    }

    for id in &queued {
        assert!(
            engine.outbox_contains(*id),
            "command {id} was lost while waiting for the upgrade"
        );
    }
    assert_eq!(
        storage.0.borrow().outbox.len(),
        queued.len(),
        "the durable outbox lost entries; an app restart would lose the work"
    );

    // The update lands. Everything queued goes out.
    engine.upgrade_completed();
    let request = engine
        .build_push(p(SERVER_CURRENT), 1_000_000)
        .expect("builds")
        .expect("a request");
    let sent: std::collections::BTreeSet<_> = request.commands.iter().map(|c| c.id).collect();
    for id in &queued {
        assert!(
            sent.contains(id),
            "command {id} never sent after the update"
        );
    }
}

/// A refused device can still read.
///
/// Someone who cannot write is better served by today's data plus an update prompt than by a blank
/// screen plus an update prompt.
#[test]
fn a_refused_device_can_still_apply_what_it_pulls() {
    let policy = VersionPolicy::window(p(SERVER_CURRENT));
    let mut server = Server::new();
    let mut rng = Rng::new(3);
    for i in 0..5 {
        server.external_change_to(&scope(), &entity(), &format!("r{i}"), &mut rng);
    }

    let storage = SimStorage::new();
    let mut engine = device(storage.clone());

    let Negotiated::Refused(envelope) = policy.negotiate(p(OLD_CLIENT)) else {
        panic!("expected a refusal");
    };
    engine.on_upgrade_required(&envelope);

    let response = server.pull(
        &PullRequest {
            protocol: p(1),
            scopes: vec![ScopeCursor {
                scope: scope(),
                cursor: Cursor::START,
            }],
            limit_bytes: None,
        },
        100,
        100_000,
    );
    let batch = response.batches.first().expect("a batch");
    engine.apply_batch(batch).expect("applies");

    assert!(
        !storage.0.borrow().rows.is_empty(),
        "a device awaiting an upgrade could not read anything"
    );
}
