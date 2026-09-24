//! CS-20: an upgraded app against a server still writing the old schema, and a device that has
//! been away for three weeks.
//!
//! # The scenario these model
//!
//! An institution updates its app. The new version understands schema v2; the change log is full
//! of rows written at v1, and some devices have been offline long enough to hold v1 commands they
//! authored before the update.
//!
//! Nothing here is exotic. It is what every schema bump looks like on the day it ships, and the
//! failure modes are both silent: rows the app cannot read, and queued work that quietly never
//! sends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::{Engine, OutboxEntry};
use credsync_protocol::{
    Command, CommandId, CommandName, ConflictClass, Cursor, EntityName, EntityRegistration,
    HexString, Payload, ProtocolVersion, PullRequest, SchemaVersion, ScopeCursor, ScopeId,
};
use credsync_sim::fakes::{SimClock, SimCompressor, SimEntropy, SimStorage, SimTransport};
use credsync_sim::{Rng, Server};
use serde_json::{Value, json};

type SimEngine = Engine<SimClock, SimEntropy, SimStorage, SimTransport, SimCompressor>;

/// What the simulated server writes. Its log is all v1.
const SERVER_SCHEMA: u16 = 1;
/// What the upgraded app understands.
const APP_SCHEMA: u16 = 2;

fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

fn v(n: u16) -> SchemaVersion {
    SchemaVersion::new(n).expect("valid schema version")
}

/// The host's v1 → v2 migration: splits `body` into `body` and a word count.
///
/// A real-shaped migration rather than a marker field — it reads existing content and derives
/// something from it, which is where a migration can actually fail.
fn v1_to_v2(value: &Value) -> Result<Value, String> {
    let mut out = value.clone();
    let body = out
        .get("v")
        .map_or_else(|| "0".to_owned(), ToString::to_string);
    out["migrated"] = json!(true);
    out["derived_len"] = json!(body.len());
    Ok(out)
}

/// A migration that refuses, for the quarantine scenario.
fn refuses(_: &Value) -> Result<Value, String> {
    Err("the v1 shape cannot be upgraded automatically".to_owned())
}

/// An upgraded app: registered at [`APP_SCHEMA`], with the host's migration registered.
fn upgraded_app(storage: SimStorage, migration: Option<credsync_core::MigrationFn>) -> SimEngine {
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
        schema_version: v(APP_SCHEMA),
    });
    engine.registry_mut().register_command(
        CommandName::new("submit_reflection").expect("valid name"),
        entity(),
    );
    if let Some(f) = migration {
        engine.migrations_mut().register(entity(), v(1), f);
    }
    engine
}

fn pull_request(cursor: u64) -> PullRequest {
    PullRequest {
        protocol: ProtocolVersion::new(1).expect("valid protocol"),
        scopes: vec![ScopeCursor {
            scope: scope(),
            cursor: Cursor::new(cursor).expect("valid cursor"),
        }],
        limit_bytes: None,
    }
}

/// Walks the log into the engine from wherever it already is, one batch at a time.
///
/// Resumes from the engine's own cursor rather than from zero. Starting at zero re-sends changes
/// the engine has already applied, which it correctly refuses with `AlreadyApplied` — the first
/// draft of this helper did exactly that, and the refusal is the client's ordering check doing its
/// job rather than a bug.
fn sync_fully(engine: &mut SimEngine, server: &Server) {
    let mut cursor = engine.scope_state(&scope()).map_or(0, |s| s.cursor.get());
    for _ in 0..200 {
        let response = server.pull(&pull_request(cursor), 8, 4_096);
        let Some(batch) = response.batches.first() else {
            return;
        };
        engine.apply_batch(batch).expect("applies");
        let next = batch.next_cursor.get();
        if next == cursor && !batch.has_more {
            return;
        }
        cursor = next;
        if !batch.has_more {
            return;
        }
    }
    panic!("the catch-up did not terminate");
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

/// A server whose log holds `n` rows, all written at the old schema.
fn server_at_v1(n: usize) -> Server {
    let mut server = Server::new();
    let mut rng = Rng::new(11);
    for i in 0..n {
        server.external_change_to(&scope(), &entity(), &format!("r{i}"), &mut rng);
    }
    server
}

// -------------------------------------------------------------------------------------------
// DoD: a v-old server against a v-new client
// -------------------------------------------------------------------------------------------

/// An upgraded app reads a log full of old-schema rows, migrating each as it applies.
#[test]
fn an_upgraded_app_migrates_every_row_the_old_server_sends() {
    let server = server_at_v1(24);
    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), Some(v1_to_v2));

    sync_fully(&mut engine, &server);

    let db = storage.0.borrow();
    assert_eq!(db.rows.len(), 24, "not every row was applied");
    assert!(
        db.quarantine.is_empty(),
        "rows were quarantined even though the migration succeeds: {:?}",
        db.quarantine
    );
    for ((_, entity_id), row) in &db.rows {
        assert_eq!(
            row.schema_version,
            v(APP_SCHEMA),
            "row {entity_id} was stored at the server's schema, not the app's"
        );
        assert_eq!(
            row.snapshot.as_value().get("migrated"),
            Some(&json!(true)),
            "row {entity_id} was stored without being migrated"
        );
    }
}

/// The server keeps writing v1 while the app runs, and every new row is migrated too.
///
/// A migration that only ran at startup, or only during bootstrap, would pass the test above and
/// fail here — which is the shape of the bug worth catching.
#[test]
fn rows_written_after_the_upgrade_are_still_migrated() {
    let mut server = server_at_v1(6);
    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), Some(v1_to_v2));

    sync_fully(&mut engine, &server);
    let first_pass = storage.0.borrow().rows.len();

    let mut rng = Rng::new(5);
    for i in 6..12 {
        server.external_change_to(&scope(), &entity(), &format!("r{i}"), &mut rng);
    }
    sync_fully(&mut engine, &server);

    let db = storage.0.borrow();
    assert!(
        db.rows.len() > first_pass,
        "the second pass applied nothing"
    );
    for ((_, entity_id), row) in &db.rows {
        assert_eq!(
            row.snapshot.as_value().get("migrated"),
            Some(&json!(true)),
            "row {entity_id} arrived after the upgrade and was not migrated"
        );
    }
}

/// With no migration registered, rows are quarantined rather than written unreadable.
///
/// This is a host that bumped `schema_version` and forgot the migration. The device must not end
/// up with v1 documents filed as v2 — that is corruption the app will read back as valid.
#[test]
fn a_missing_migration_quarantines_rather_than_corrupting() {
    let server = server_at_v1(8);
    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), None);

    sync_fully(&mut engine, &server);

    let db = storage.0.borrow();
    assert_eq!(
        db.rows.len(),
        0,
        "unmigratable rows were written into the live table"
    );
    assert_eq!(db.quarantine.len(), 8, "the rows were not all quarantined");
    for (_, snapshot, schema_version, _) in &db.quarantine {
        assert_eq!(
            *schema_version,
            v(SERVER_SCHEMA),
            "the quarantined row lost the version it was actually written under"
        );
        assert!(
            snapshot.as_value().get("migrated").is_none(),
            "a half-migrated document reached quarantine; the original is gone"
        );
    }
}

/// A failing migration quarantines too, and keeps the original bytes.
#[test]
fn a_failing_migration_quarantines_the_original() {
    let server = server_at_v1(4);
    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), Some(refuses));

    sync_fully(&mut engine, &server);

    let db = storage.0.borrow();
    assert_eq!(db.quarantine.len(), 4);
    assert!(
        db.quarantine
            .iter()
            .all(|(_, _, _, reason)| reason.contains("cannot be upgraded automatically")),
        "the host's reason did not reach the quarantine record"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: three-week-offline device catch-up
// -------------------------------------------------------------------------------------------

/// A device away for three weeks catches up, migrating a long backlog, and sends what it queued.
///
/// `docs/spec.md` §3.2's headline case. The device holds commands authored under v1 *and* faces a
/// log of v1 rows written while it was away, so both halves of §7 run at once — which is exactly
/// the combination a real upgrade produces and which testing each half alone would miss.
#[test]
fn a_three_week_offline_device_catches_up_and_sends_its_backlog() {
    // Three weeks of somebody else's writing.
    let server = server_at_v1(120);

    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), Some(v1_to_v2));

    // Work the user did on the plane, authored under the old schema before the app updated.
    let queued: Vec<CommandId> = (0..5)
        .map(|n| {
            let command = command_n(n);
            let id = command.id;
            engine
                .enqueue(OutboxEntry::new(command, v(1)))
                .expect("queues");
            id
        })
        .collect();

    // Back online: walk the whole backlog.
    sync_fully(&mut engine, &server);

    let applied = storage.0.borrow().rows.len();
    assert_eq!(applied, 120, "the backlog did not fully apply");
    assert!(
        storage.0.borrow().quarantine.is_empty(),
        "rows were quarantined during catch-up"
    );

    // And the queued work goes out, migrated forward.
    let request = engine
        .build_push(ProtocolVersion::new(1).expect("valid protocol"), 1_000_000)
        .expect("builds")
        .expect("a request");

    assert_eq!(
        request.commands.len(),
        queued.len(),
        "not every queued command was sent"
    );
    for command in &request.commands {
        assert_eq!(
            command.payload.as_value().get("migrated"),
            Some(&json!(true)),
            "command {} went out at the schema it was authored under",
            command.id
        );
    }
}

/// The offline backlog is never dropped, even when it cannot be migrated.
///
/// The worst case for a user: three weeks of work, an app update that cannot migrate it, and a
/// push that has to refuse. The commands must still be there afterwards.
#[test]
fn an_unmigratable_offline_backlog_is_held_not_lost() {
    let storage = SimStorage::new();
    let mut engine = upgraded_app(storage.clone(), Some(refuses));

    let queued: Vec<CommandId> = (0..5)
        .map(|n| {
            let command = command_n(n);
            let id = command.id;
            engine
                .enqueue(OutboxEntry::new(command, v(1)))
                .expect("queues");
            id
        })
        .collect();

    let request = engine
        .build_push(ProtocolVersion::new(1).expect("valid protocol"), 1_000_000)
        .expect("builds");

    assert!(
        request.is_none_or(|r| r.commands.is_empty()),
        "an unmigratable command was sent anyway"
    );
    for id in &queued {
        assert!(
            engine.outbox_contains(*id),
            "command {id} was dropped rather than held"
        );
    }
    assert_eq!(
        storage.0.borrow().outbox.len(),
        queued.len(),
        "the durable outbox lost entries the engine still believes it has"
    );
}
