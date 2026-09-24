//! CS-19: initial-snapshot bootstrap.
//!
//! # What bootstrap has to survive
//!
//! A device with nothing walks a scope that may be a year old, over a network that will interrupt
//! it, while other people keep writing. Three things must hold at the end:
//!
//! 1. It ends up holding exactly what the server says it should.
//! 2. Interrupting it halfway loses nothing — it resumes from `after`.
//! 3. Writes landing *during* the bootstrap are neither lost nor applied twice.
//!
//! The third is where the interesting bug lives, and it is a **delete**. A row delivered on page
//! one and deleted while page two is computed is no longer a live row, so a bootstrap that returned
//! live rows would never resend it — and its tombstone sits below the `next_cursor` the device
//! joins the log at, so the log would never deliver it either. The device would hold a deleted row
//! permanently.
//!
//! That is why `docs/spec.md` §3.1 defines bootstrap as the **compacted log** rather than a list of
//! live rows, and why `a_row_deleted_during_bootstrap_does_not_survive_it` exists.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::Engine;
use credsync_protocol::{
    BootstrapRequest, ConflictClass, Cursor, EntityId, EntityName, EntityRegistration, Op,
    ProtocolVersion, SchemaVersion, ScopeDigest, ScopeId,
};
use credsync_sim::fakes::{SimClock, SimCompressor, SimEntropy, SimStorage, SimTransport};
use credsync_sim::{Rng, Server};

/// The compressed byte budget these tests serve under.
///
/// Very small on purpose. The simulator's snapshots are `{"v": <seq>}`, so a change encodes to
/// well under two hundred bytes and compresses to a third of that — at anything near the real
/// 100 KB budget every fixture below fits in a single page, and the pagination, the resume and
/// the mid-bootstrap write all go untested while the suite stays green.
///
/// The first draft used 512 and did exactly that. The `has_more` assertions in these tests exist
/// to catch it, and did.
const BUDGET: usize = 128;

fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

fn request(after: u64) -> BootstrapRequest {
    BootstrapRequest {
        protocol: ProtocolVersion::new(1).expect("valid protocol"),
        scope: scope(),
        after: Cursor::new(after).expect("valid cursor"),
    }
}

/// A server holding `rows` rows in one scope, each written once.
fn server_with(rows: usize) -> (Server, Rng) {
    let mut server = Server::new();
    let mut rng = Rng::new(7);
    for n in 0..rows {
        server.external_change_to(&scope(), &entity(), &format!("r{n}"), &mut rng);
    }
    (server, rng)
}

type SimEngine = Engine<SimClock, SimEntropy, SimStorage, SimTransport, SimCompressor>;

fn register(registry: &mut credsync_core::Registry) {
    registry.register_entity(EntityRegistration {
        entity: entity(),
        scope: scope(),
        conflict_class: ConflictClass::OwnerDraft,
        schema_version: SchemaVersion::new(1).expect("valid schema version"),
    });
}

/// An engine over the given storage. A device that has never synced when the storage is empty.
fn device_on(storage: SimStorage) -> SimEngine {
    let mut engine = Engine::new(
        SimClock::new(1_756_137_600_000, 0, 0),
        SimEntropy::new(1),
        storage,
        SimTransport::new(),
        SimCompressor::with_ratio(3),
    );
    register(engine.registry_mut());
    engine
}

/// A fresh device and the storage behind it.
fn fresh_device() -> (SimEngine, SimStorage) {
    let storage = SimStorage::new();
    (device_on(storage.clone()), storage)
}

/// Runs a bootstrap to completion, returning how many pages it took.
fn bootstrap_fully(engine: &mut SimEngine, server: &Server) -> usize {
    let mut after = 0u64;
    let mut pages = 0;
    loop {
        let response = server.bootstrap(&request(after), BUDGET);
        engine.apply_bootstrap(&response).expect("applies");
        pages += 1;
        after = response.next_cursor.get();
        if !response.has_more {
            return pages;
        }
        assert!(pages < 100, "bootstrap did not terminate");
    }
}

// -------------------------------------------------------------------------------------------
// A new device ends up holding what the server says it should
// -------------------------------------------------------------------------------------------

#[test]
fn a_new_device_bootstraps_to_the_servers_state() {
    let (server, _) = server_with(12);
    let (mut engine, storage) = fresh_device();

    let pages = bootstrap_fully(&mut engine, &server);
    assert!(
        pages > 1,
        "the whole scope fit in one page, so pagination was never exercised"
    );

    let cursor = storage
        .0
        .borrow()
        .cursors
        .get(&scope())
        .copied()
        .expect("a cursor");
    let expected = server.rows_at(&scope(), cursor.get());
    assert_eq!(
        storage.0.borrow().rows.len(),
        expected.len(),
        "the device holds a different number of rows than the server says it should"
    );
    for (entity, entity_id, version) in expected {
        let held = storage
            .0
            .borrow()
            .rows
            .get(&(entity.clone(), entity_id.clone()))
            .map(|r| r.row_version);
        assert_eq!(
            held,
            Some(version),
            "row {entity_id} is at {held:?}, server says {version:?}"
        );
    }
}

/// Bootstrap sends each row once, not once per edit.
///
/// The reason it is a separate endpoint at all. A scope with a long history must not make a new
/// device replay every version of every row.
#[test]
fn bootstrap_sends_one_entry_per_row_not_one_per_edit() {
    let mut server = Server::new();
    let mut rng = Rng::new(3);
    // Four rows, each edited five times: twenty log entries, four rows.
    for _ in 0..5 {
        for n in 0..4 {
            server.external_change_to(&scope(), &entity(), &format!("r{n}"), &mut rng);
        }
    }

    let mut after = 0u64;
    let mut delivered = 0usize;
    loop {
        let response = server.bootstrap(&request(after), 100_000);
        delivered += response.changes.len();
        after = response.next_cursor.get();
        if !response.has_more {
            break;
        }
    }

    assert_eq!(
        delivered, 4,
        "bootstrap delivered {delivered} entries for 4 rows — it is replaying history, not \
         compacting it"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: resumable mid-way
// -------------------------------------------------------------------------------------------

/// A bootstrap killed halfway resumes from `after` and converges.
///
/// The device keeps whatever it committed, and asks again from its cursor. Nothing is re-applied
/// and nothing is skipped — the cursor rides in the same transaction as the rows it covers.
#[test]
fn a_bootstrap_killed_halfway_resumes_without_loss() {
    let (server, _) = server_with(12);
    let (mut engine, storage) = fresh_device();

    // Two pages, then "die".
    let mut after = 0u64;
    for _ in 0..2 {
        let response = server.bootstrap(&request(after), BUDGET);
        engine.apply_bootstrap(&response).expect("applies");
        after = response.next_cursor.get();
        assert!(
            response.has_more,
            "the fixture must need more than two pages"
        );
    }
    let partial_rows = storage.0.borrow().rows.len();
    let resume_from = storage
        .0
        .borrow()
        .cursors
        .get(&scope())
        .copied()
        .expect("cursor");
    assert!(partial_rows > 0, "nothing was committed before the kill");

    // A new engine over the SAME storage: a restarted process, which is what a kill looks like.
    // Nothing carries over in memory — whatever the database holds is the truth.
    let mut resumed = device_on(storage.clone());
    let restored_digest = storage
        .0
        .borrow()
        .digests
        .get(&scope())
        .and_then(|h| u128::from_str_radix(h.as_str(), 16).ok())
        .map_or(ScopeDigest::EMPTY, ScopeDigest::from_raw);
    resumed.restore_scope(
        scope(),
        credsync_core::ScopeState::restored(resume_from, restored_digest),
    );

    let mut after = resume_from.get();
    loop {
        let response = server.bootstrap(&request(after), BUDGET);
        resumed.apply_bootstrap(&response).expect("applies");
        after = response.next_cursor.get();
        if !response.has_more {
            break;
        }
    }

    let cursor = storage
        .0
        .borrow()
        .cursors
        .get(&scope())
        .copied()
        .expect("cursor");
    let expected = server.rows_at(&scope(), cursor.get());
    assert_eq!(
        storage.0.borrow().rows.len(),
        expected.len(),
        "the resumed bootstrap did not end up with the server's rows"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: concurrent writes during bootstrap
// -------------------------------------------------------------------------------------------

/// A row updated mid-bootstrap arrives at its latest version, once.
#[test]
fn a_row_updated_during_bootstrap_converges_to_its_latest_version() {
    let (mut server, mut rng) = server_with(10);
    let (mut engine, storage) = fresh_device();

    let first = server.bootstrap(&request(0), BUDGET);
    engine.apply_bootstrap(&first).expect("applies");
    assert!(first.has_more, "the fixture must need more than one page");
    let delivered: Vec<EntityId> = first.changes.iter().map(|c| c.entity_id.clone()).collect();
    let touched = delivered.first().cloned().expect("page one sent rows");

    // Somebody edits a row that page one already delivered.
    server.external_change_to(&scope(), &entity(), touched.as_str(), &mut rng);

    let mut after = first.next_cursor.get();
    loop {
        let response = server.bootstrap(&request(after), BUDGET);
        engine.apply_bootstrap(&response).expect("applies");
        after = response.next_cursor.get();
        if !response.has_more {
            break;
        }
    }

    let cursor = storage
        .0
        .borrow()
        .cursors
        .get(&scope())
        .copied()
        .expect("cursor");
    for (entity, entity_id, version) in server.rows_at(&scope(), cursor.get()) {
        assert_eq!(
            storage
                .0
                .borrow()
                .rows
                .get(&(entity, entity_id.clone()))
                .map(|r| r.row_version),
            Some(version),
            "row {entity_id} did not converge to the server's version"
        );
    }
}

/// **A row deleted during a bootstrap must not survive it.**
///
/// The test this slice exists for. The row is delivered on page one, then deleted while later
/// pages are still being fetched.
///
/// Under the old "bootstrap returns live rows" shape this was unfixable: the row is no longer live
/// so bootstrap cannot resend it, and its tombstone sits below the `next_cursor` the device joins
/// the log at, so the log cannot deliver it either. The device would hold a deleted row forever.
///
/// The compacted log carries the tombstone instead, because a delete *is* that row's latest change.
#[test]
fn a_row_deleted_during_bootstrap_does_not_survive_it() {
    let (mut server, _) = server_with(10);
    let (mut engine, storage) = fresh_device();

    let first = server.bootstrap(&request(0), BUDGET);
    engine.apply_bootstrap(&first).expect("applies");
    assert!(first.has_more, "the fixture must need more than one page");

    let doomed = first
        .changes
        .first()
        .map(|c| c.entity_id.clone())
        .expect("page one sent rows");
    assert!(
        storage
            .0
            .borrow()
            .rows
            .contains_key(&(entity(), doomed.clone())),
        "the device should be holding the row before it is deleted"
    );

    // Deleted after page one, while the bootstrap is still running.
    server.external_delete(&scope(), &entity(), doomed.as_str());

    let mut after = first.next_cursor.get();
    let mut saw_tombstone = false;
    loop {
        let response = server.bootstrap(&request(after), BUDGET);
        saw_tombstone |= response
            .changes
            .iter()
            .any(|c| c.entity_id == doomed && c.op == Op::Delete);
        engine.apply_bootstrap(&response).expect("applies");
        after = response.next_cursor.get();
        if !response.has_more {
            break;
        }
    }

    assert!(
        saw_tombstone,
        "the bootstrap never carried a tombstone for {doomed}, so the device cannot have learned \
         of the delete"
    );
    assert!(
        !storage
            .0
            .borrow()
            .rows
            .contains_key(&(entity(), doomed.clone())),
        "row {doomed} was deleted during the bootstrap and the device is still holding it"
    );
}

/// A fresh bootstrap carries no historical tombstones.
///
/// `after = 0` means the device holds nothing, so a delete from last year corrects nothing and is
/// pure noise — on a scope with heavy churn it could be most of the payload.
#[test]
fn a_fresh_bootstrap_omits_historical_tombstones() {
    let (mut server, _) = server_with(6);
    server.external_delete(&scope(), &entity(), "r0");
    server.external_delete(&scope(), &entity(), "r1");

    let response = server.bootstrap(&request(0), 100_000);
    assert!(
        response.changes.iter().all(|c| c.op != Op::Delete),
        "a fresh bootstrap carried a tombstone for a row the device never had"
    );
    assert!(
        !response.changes.is_empty(),
        "the fixture should still have live rows to send"
    );
}

// -------------------------------------------------------------------------------------------
// DoD: the same byte budget as ordinary pull
// -------------------------------------------------------------------------------------------

/// Bootstrap is byte-budgeted, and one oversized row still goes rather than stalling.
///
/// Both come from `fill_batch` — the same function pull serves from — so this asserts the wiring
/// rather than re-testing the rule.
#[test]
fn bootstrap_respects_the_compressed_byte_budget() {
    let (server, _) = server_with(20);

    let generous = server.bootstrap(&request(0), 100_000);
    let mean = server.bootstrap(&request(0), 256);

    assert!(
        mean.changes.len() < generous.changes.len(),
        "a smaller budget returned as much as a large one ({} vs {}), so the budget does not bind",
        mean.changes.len(),
        generous.changes.len()
    );
    assert!(
        !mean.changes.is_empty(),
        "a tight budget returned nothing at all; one change must always go or the scope stalls"
    );
    assert!(mean.has_more, "a truncated bootstrap must say so");
}

/// A partial bootstrap does not report divergence.
///
/// The digest on a bootstrap response covers the whole scope. A device holding page one of four
/// will not match it, and calling that divergence would fire the detector on every normal
/// bootstrap — which is how a detector gets switched off.
#[test]
fn a_partial_bootstrap_is_not_reported_as_divergence() {
    let (server, _) = server_with(12);
    let (mut engine, _storage) = fresh_device();

    let first = server.bootstrap(&request(0), BUDGET);
    assert!(first.has_more, "the fixture must need more than one page");

    let applied = engine.apply_bootstrap(&first).expect("applies");
    assert!(
        !applied.diverged,
        "a partial bootstrap was reported as divergence"
    );
}
