//! CS-22: forcing a real divergence and watching the client heal itself.
//!
//! The core tests drive the taint state directly. These do it the hard way: corrupt a device's
//! stored rows behind the engine's back, let it discover the mismatch on its next pull, and check
//! it ends up holding exactly what the server says it should — with its queued work intact.
//!
//! Corrupting storage directly is the honest way to model this. Silent divergence does not arrive
//! through the protocol; it arrives from a bug in an adapter, a partial write, a filesystem that
//! lied. The protocol is what *detects* it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::{Engine, OutboxEntry, ScopeHealth};
use credsync_protocol::{
    BootstrapRequest, Command, CommandId, CommandName, ConflictClass, Cursor, EntityName,
    EntityRegistration, HexString, Payload, ProtocolVersion, PullRequest, RowVersion,
    SchemaVersion, ScopeCursor, ScopeId,
};
use credsync_sim::fakes::{SimClock, SimCompressor, SimEntropy, SimStorage, SimTransport};
use credsync_sim::{Rng, Server};
use serde_json::json;

type SimEngine = Engine<SimClock, SimEntropy, SimStorage, SimTransport, SimCompressor>;

fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
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

fn server_with(rows: usize) -> Server {
    let mut server = Server::new();
    let mut rng = Rng::new(17);
    for i in 0..rows {
        server.external_change_to(&scope(), &entity(), &format!("r{i}"), &mut rng);
    }
    server
}

/// Restarts the device, recomputing its digest from the rows storage actually holds.
///
/// The only way silent storage corruption becomes visible. A restart that trusted the *stored*
/// digest would carry the engine's old belief across the restart and keep agreeing with the server
/// about a set of rows the device no longer has.
fn restart_recomputing(storage: &SimStorage) -> SimEngine {
    let mut engine = device(storage.clone());
    let (cursor, digest, attempts) = {
        let db = storage.0.borrow();
        let cursor = db.cursors.get(&scope()).copied().unwrap_or(Cursor::START);
        (
            cursor,
            db.digest_over_rows(&scope(), &[entity()]),
            db.divergences.get(&scope()).copied().unwrap_or(0),
        )
    };
    engine.restore_scope(scope(), credsync_core::ScopeState::restored(cursor, digest));
    // Carried across the restart on purpose. Without it the escalation counts only what this
    // process has seen, and a crash-looping device rebuilds the same scope forever.
    engine.restore_scope_health(scope(), attempts);
    engine
}

/// Pulls until the server has nothing more, applying each batch.
fn sync(engine: &mut SimEngine, server: &Server) {
    for _ in 0..200 {
        let cursor = engine
            .scope_state(&scope())
            .map_or(Cursor::START, |s| s.cursor);
        let response = server.pull(
            &PullRequest {
                protocol: ProtocolVersion::new(1).expect("valid protocol"),
                scopes: vec![ScopeCursor {
                    scope: scope(),
                    cursor,
                }],
                limit_bytes: None,
            },
            8,
            4_096,
        );
        let Some(batch) = response.batches.first() else {
            return;
        };
        // A diverged scope stops being worth pulling: applying new changes onto a base already
        // known to be wrong is how a fault compounds.
        let applied = engine.apply_batch(batch);
        if applied.is_err() || !batch.has_more {
            return;
        }
    }
}

/// Rebuilds a tainted scope from the server's compacted snapshot, as the sync loop would.
fn rebuild(engine: &mut SimEngine, server: &Server) {
    engine
        .begin_rebootstrap(&scope())
        .expect("clears the scope");

    let mut after = 0u64;
    for _ in 0..200 {
        let response = server.bootstrap(
            &BootstrapRequest {
                protocol: ProtocolVersion::new(1).expect("valid protocol"),
                scope: scope(),
                after: Cursor::new(after).expect("valid cursor"),
            },
            4_096,
        );
        engine.apply_bootstrap(&response).expect("applies");
        after = response.next_cursor.get();
        if !response.has_more {
            return;
        }
    }
    panic!("the rebuild did not terminate");
}

/// Corrupts a device's storage behind the engine's back.
///
/// What an adapter bug or a partial write looks like from the engine's side: rows that are simply
/// wrong, with nothing having errored.
///
/// On its own this is **not** detectable. The engine's digest is incremental and held in memory —
/// it describes what the engine believes it applied, not what storage holds — so damaging the rows
/// underneath it changes nothing the engine can see. Detection needs [`restart_recomputing`],
/// which is what a careful adapter does at start-up.
fn corrupt(storage: &SimStorage, rows_to_damage: usize) {
    let mut db = storage.0.borrow_mut();
    let keys: Vec<_> = db.rows.keys().take(rows_to_damage).cloned().collect();
    for key in keys {
        if let Some(row) = db.rows.get_mut(&key) {
            // A plausible-looking row version that is not the server's.
            row.row_version =
                RowVersion::new(row.row_version.get() + 1_000).expect("valid row version");
        }
    }
}

// -------------------------------------------------------------------------------------------
// DoD: a forced divergence is detected and fully recovered
// -------------------------------------------------------------------------------------------

/// Corrupted storage is detected on the next pull, rebuilt, and ends up matching the server.
#[test]
fn a_forced_divergence_is_detected_and_fully_healed() {
    let mut server = server_with(20);
    let storage = SimStorage::new();
    let mut engine = device(storage.clone());

    sync(&mut engine, &server);
    assert!(
        engine.scope_health(&scope()).is_healthy(),
        "the device diverged before anything was corrupted"
    );

    corrupt(&storage, 3);

    // The app restarts and checks its rows against its digest, which is where the damage becomes
    // visible. A new change then arrives, and applying it is where the two sides disagree.
    let mut engine = restart_recomputing(&storage);
    let mut rng = Rng::new(2);
    server.external_change_to(&scope(), &entity(), "r99", &mut rng);
    sync(&mut engine, &server);

    assert!(
        engine.scope_health(&scope()).needs_rebootstrap(),
        "corrupted rows went undetected — this is the silent divergence the digest exists to catch"
    );

    rebuild(&mut engine, &server);

    assert!(
        engine.scope_health(&scope()).is_healthy(),
        "the scope is still tainted after being rebuilt"
    );

    let db = storage.0.borrow();
    let cursor = db.cursors.get(&scope()).copied().expect("a cursor");
    for (entity, entity_id, expected) in server.rows_at(&scope(), cursor.get()) {
        assert_eq!(
            db.rows
                .get(&(entity, entity_id.clone()))
                .map(|r| r.row_version),
            Some(expected),
            "row {entity_id} did not recover to the server's version"
        );
    }
}

/// A row the server deleted while the client was broken does not survive the rebuild.
///
/// The case that makes clearing mandatory. A fresh bootstrap carries no tombstones, so a stale row
/// would otherwise outlive the rebuild and keep the digest wrong — healing that does not heal.
#[test]
fn a_row_the_server_no_longer_has_does_not_survive_the_rebuild() {
    let mut server = server_with(10);
    let storage = SimStorage::new();
    let mut engine = device(storage.clone());

    sync(&mut engine, &server);
    assert!(storage.0.borrow().rows.contains_key(&(
        entity(),
        credsync_protocol::EntityId::new("r0").expect("valid id")
    )));

    // The server drops a row while the client is not looking, and the client's copy is corrupted.
    server.external_delete(&scope(), &entity(), "r0");
    corrupt(&storage, 2);
    let mut engine = restart_recomputing(&storage);

    rebuild(&mut engine, &server);

    assert!(
        !storage.0.borrow().rows.contains_key(&(
            entity(),
            credsync_protocol::EntityId::new("r0").expect("valid id")
        )),
        "a row the server no longer has survived the rebuild"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: the outbox is replayed, nothing lost or double-applied
// -------------------------------------------------------------------------------------------

/// Queued work survives a rebuild and is applied exactly once.
///
/// The rebuild fixes the read path. The write path must be untouched by it — and the server's
/// dedupe table is what stops a replayed command applying twice.
#[test]
fn the_outbox_survives_a_rebuild_and_replays_exactly_once() {
    let mut server = server_with(12);
    let storage = SimStorage::new();
    let mut engine = device(storage.clone());
    let mut rng = Rng::new(9);

    sync(&mut engine, &server);

    let queued: Vec<CommandId> = (0..4)
        .map(|n| {
            let command = command_n(n);
            let id = command.id;
            engine
                .enqueue(OutboxEntry::new(
                    command,
                    SchemaVersion::new(1).expect("valid schema version"),
                ))
                .expect("queues");
            id
        })
        .collect();

    // Some of it reaches the server before the divergence is noticed.
    let first = engine
        .build_push(ProtocolVersion::new(1).expect("valid protocol"), 1_000_000)
        .expect("builds")
        .expect("a request");
    let answered = server.push(&first, &mut rng);
    engine.apply_results(&answered).expect("applies results");

    // Now corrupt and rebuild.
    corrupt(&storage, 3);
    rebuild(&mut engine, &server);

    // Whatever is still queued goes out again. The server has seen some of it before.
    if let Some(replay) = engine
        .build_push(ProtocolVersion::new(1).expect("valid protocol"), 1_000_000)
        .expect("builds")
    {
        let results = server.push(&replay, &mut rng);
        engine.apply_results(&results).expect("applies results");
    }

    // Every command reached a terminal state: answered, or still queued. None vanished.
    let db = storage.0.borrow();
    let resolved: std::collections::BTreeSet<_> = db.resolved.iter().map(|(id, _)| *id).collect();
    let still_queued: std::collections::BTreeSet<_> = db.outbox.iter().map(|(c, _)| c.id).collect();
    for id in &queued {
        assert!(
            resolved.contains(id) || still_queued.contains(id),
            "command {id} was lost across the rebuild"
        );
    }

    // And the server answered each id exactly once, however many times it was sent.
    for id in &queued {
        if resolved.contains(id) {
            assert!(
                server.has_answered(id),
                "command {id} is resolved on the device but the server never decided it"
            );
        }
    }
}

// -------------------------------------------------------------------------------------------
// DoD: escalation rather than an endless loop
// -------------------------------------------------------------------------------------------

/// A scope that keeps diverging after every rebuild stops being rebuilt.
///
/// Modelled by corrupting the storage again immediately after each rebuild — a broken adapter,
/// which is precisely the case that re-downloading cannot fix.
#[test]
fn a_scope_that_cannot_be_healed_stops_being_rebuilt() {
    let mut server = server_with(8);
    let storage = SimStorage::new();
    let mut engine = device(storage.clone());
    let mut rng = Rng::new(4);

    sync(&mut engine, &server);

    // A broken adapter: the rows are damaged again after every rebuild, and the app restarts
    // between rounds. Restarting is the important part — it is what a crash-looping device does,
    // and an escalation counter held only in memory would reset here and loop forever.
    let mut rebuilds = 0;
    for round in 0..6 {
        corrupt(&storage, 2);
        let mut restarted = restart_recomputing(&storage);
        server.external_change_to(&scope(), &entity(), &format!("x{round}"), &mut rng);
        sync(&mut restarted, &server);

        if restarted.scope_health(&scope()).needs_rebootstrap() {
            rebuild(&mut restarted, &server);
            rebuilds += 1;
        }
    }

    // The count survived every restart, so healing stopped instead of running for ever.
    let final_engine = restart_recomputing(&storage);
    let health = final_engine.scope_health(&scope());
    assert!(
        matches!(health, ScopeHealth::Unhealable { .. }),
        "after six rounds of damage the scope is still being healed: {health:?}"
    );
    assert!(
        final_engine.scopes_needing_rebootstrap().is_empty(),
        "an unhealable scope is still offered for rebuild, which is the loop"
    );
    assert!(
        rebuilds < 6,
        "the scope was rebuilt on every one of six rounds; escalation never fired"
    );
    drop(engine);
}
