//! CS-9: conflict application per entity class. `docs/spec.md` §6.
//!
//! The design's claim is that a policy here is *"a tested property rather than documentation"*.
//! These are those tests.
//!
//! # The constraint behind every case below
//!
//! Class policy may never make the client hold different rows from the server. The scope digest
//! is computed over the rows a client holds, so a rule that caused the client to skip a change
//! the server applied would put the two permanently out of step: divergence on every pull, a
//! re-bootstrap, the same change again, skipped again, forever.
//!
//! So the server stays the source of truth for row content in every class. What the classes
//! govern is which commands may be *queued*, whether a losing edit is *recovered*, and whether a
//! contract the server broke is *reported*.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use credsync_core::{ApplyError, Lww, OutboxEntry, OutboxError, RegistryError, lww_winner};
use credsync_protocol::{ConflictClass, EntityName, RowVersion};

fn rv(v: u64) -> RowVersion {
    RowVersion::new(v).expect("valid row_version")
}

// -------------------------------------------------------------------------------------------
// DoD 1 — the registry refuses commands against server-authoritative entities
// -------------------------------------------------------------------------------------------

/// `docs/spec.md` §6: institution truth is pull-only, refused by the registry, not by convention.
///
/// Refused here rather than at the server on purpose. A client that could queue such a command
/// could change its own grade, and a modified client simply would not ask the server's permission.
#[test]
fn a_command_targeting_a_server_authoritative_entity_is_refused() {
    let (mut engine, storage) = new_engine();

    let err = engine
        .enqueue(OutboxEntry::new(named_command(1, "amend_grade"), schema()))
        .expect_err("grades are pull-only");

    assert_eq!(
        err,
        OutboxError::Registry(RegistryError::ServerAuthoritative {
            entity: EntityName::new("grades").unwrap()
        })
    );
    assert_eq!(engine.outbox_len(), 0);
    storage.with(|s| assert!(s.queued.is_empty(), "it never reached storage either"));
}

#[test]
fn commands_targeting_the_other_two_classes_are_accepted() {
    let (mut engine, _storage) = new_engine();

    engine
        .enqueue(OutboxEntry::new(
            named_command(1, "submit_reflection"),
            schema(),
        ))
        .expect("owner drafts accept commands");
    engine
        .enqueue(OutboxEntry::new(
            named_command(2, "add_submission"),
            schema(),
        ))
        .expect("append-only streams accept commands");

    assert_eq!(engine.outbox_len(), 2);
}

/// An unregistered command is refused, not waved through.
///
/// Defaulting to "permitted" would mean this protection applies only to entities someone
/// remembered to register — which is the convention it exists to replace.
#[test]
fn an_unregistered_command_is_refused() {
    let (mut engine, _storage) = new_engine_unregistered();

    let err = engine
        .enqueue(entry(1, 10))
        .expect_err("an empty registry accepts nothing");

    assert!(matches!(
        err,
        OutboxError::Registry(RegistryError::UnknownCommand { .. })
    ));
}

#[test]
fn the_registry_reports_each_class_it_was_given() {
    let (engine, _storage) = new_engine();
    let r = engine.registry();

    assert_eq!(
        r.class_of(&EntityName::new("grades").unwrap()),
        Some(ConflictClass::ServerAuthoritative)
    );
    assert_eq!(
        r.class_of(&EntityName::new("reflections").unwrap()),
        Some(ConflictClass::OwnerDraft)
    );
    assert_eq!(
        r.class_of(&EntityName::new("submissions").unwrap()),
        Some(ConflictClass::AppendOnly)
    );
}

// -------------------------------------------------------------------------------------------
// DoD 2 and 3 — LWW is decided by row_version, and a skewed clock wins nothing
// -------------------------------------------------------------------------------------------

#[test]
fn lww_is_decided_by_row_version() {
    assert_eq!(lww_winner(rv(9), rv(4)), Lww::Incoming);
    assert_eq!(lww_winner(rv(4), rv(9)), Lww::Stored);
    assert_eq!(lww_winner(rv(4), rv(4)), Lww::Tie);
    assert!(lww_winner(rv(9), rv(4)).incoming_wins());
    assert!(!lww_winner(rv(4), rv(9)).incoming_wins());
}

/// DoD 3: a device whose clock is three days fast does not win on that basis.
///
/// The strongest form of this test is structural rather than behavioural: `lww_winner` **takes no
/// timestamp argument at all**. A function that cannot see the clock cannot be corrupted by it,
/// which beats a comment asking the reader not to use it.
///
/// So the test below constructs exactly the situation — a command authored with a `client_ts`
/// three days in the future, against a stored row with a *higher* `row_version` — and asserts the
/// skewed side still loses. `docs/spec.md` §6: `client_ts` is a tiebreaker hint only, because
/// device clocks lie.
#[test]
fn a_clock_skewed_three_days_forward_does_not_win_an_lww_conflict() {
    const THREE_DAYS_MS: i64 = 3 * 24 * 60 * 60 * 1000;

    let honest = command(1, 8);
    let mut skewed = command(2, 8);
    skewed.client_ts = honest.client_ts + THREE_DAYS_MS;

    assert!(
        skewed.client_ts > honest.client_ts,
        "the skewed device really does claim to be later"
    );

    // The skewed device's edit carries the LOWER server-assigned version.
    let skewed_version = rv(4);
    let honest_version = rv(9);

    assert_eq!(
        lww_winner(skewed_version, honest_version),
        Lww::Stored,
        "a three-day-fast clock must not win: row_version decides, and only row_version"
    );
}

// -------------------------------------------------------------------------------------------
// DoD 4 — the losing version is stored as a recovered draft
// -------------------------------------------------------------------------------------------

/// `docs/spec.md` §6: the losing version returns to the device and is stored as a recovered
/// draft. Silent loss is a protocol violation, not a tradeoff.
///
/// A superseded command is exactly that losing version: the server applied last-write-wins and
/// this edit lost. Its payload is the user's text, and it exists nowhere else on the device once
/// the entry leaves the outbox.
#[test]
fn a_superseded_owner_draft_is_recovered_not_lost() {
    let (mut engine, storage) = new_engine();
    engine
        .enqueue(OutboxEntry::new(
            named_command(1, "submit_reflection"),
            schema(),
        ))
        .expect("enqueues");

    engine
        .apply_results(&push_response(vec![superseded(1)]))
        .expect("resolves");

    storage.with(|s| {
        let draft = s
            .recovered
            .iter()
            .find(|(cmd, _, _)| *cmd == command_id(1))
            .expect("the losing edit must be retrievable");
        assert_eq!(draft.1.as_str(), "reflections");
        assert!(
            !draft.2.as_value().is_null(),
            "the user's content is preserved, not a placeholder"
        );
    });
}

/// The draft and the resolution commit together.
///
/// Two transactions, and a crash in between loses exactly the work this mechanism exists to keep.
#[test]
fn the_recovered_draft_and_the_resolution_are_one_transaction() {
    let (mut engine, storage) = new_engine();
    engine
        .enqueue(OutboxEntry::new(
            named_command(1, "submit_reflection"),
            schema(),
        ))
        .expect("enqueues");
    let before = storage.with(|s| s.attempts);

    engine
        .apply_results(&push_response(vec![superseded(1)]))
        .expect("resolves");

    storage.with(|s| {
        assert_eq!(s.attempts - before, 1, "one transaction, not two");
        assert_eq!(s.recovered.len(), 1);
        assert_eq!(s.resolved.len(), 1);
    });
}

/// Append-only commands have no draft to recover — their entries are never replaced.
#[test]
fn a_superseded_append_only_command_saves_no_draft() {
    let (mut engine, storage) = new_engine();
    engine
        .enqueue(OutboxEntry::new(
            named_command(1, "add_submission"),
            schema(),
        ))
        .expect("enqueues");

    engine
        .apply_results(&push_response(vec![superseded(1)]))
        .expect("resolves");

    storage.with(|s| {
        assert!(s.recovered.is_empty(), "nothing to recover for a stream");
        assert_eq!(s.resolved.len(), 1, "but it is still resolved");
    });
}

// -------------------------------------------------------------------------------------------
// DoD 5 — append-only entities are never overwritten
// -------------------------------------------------------------------------------------------

#[test]
fn a_second_version_of_an_append_only_row_is_refused() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![upsert_of("submissions", 1, "sub:a", 1)]))
        .expect("the first version lands");

    let err = engine
        .apply_batch(&batch(vec![upsert_of("submissions", 2, "sub:a", 2)]))
        .expect_err("append-only entries are never replaced");

    assert!(matches!(
        err,
        ApplyError::AppendOnlyOverwrite {
            stored: 1,
            incoming: 2,
            ..
        }
    ));
    storage.with(|s| {
        assert_eq!(
            s.row(&EntityName::new("submissions").unwrap(), &id("sub:a"))
                .unwrap()
                .row_version
                .get(),
            1,
            "the original entry is untouched"
        );
    });
}

#[test]
fn new_append_only_rows_apply_normally() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![
            upsert_of("submissions", 1, "sub:a", 1),
            upsert_of("submissions", 2, "sub:b", 1),
            upsert_of("submissions", 3, "sub:c", 1),
        ]))
        .expect("distinct entries are the normal case");

    storage.with(|s| assert_eq!(s.row_count(), 3));
}

/// The same version arriving twice is a duplicate, not an overwrite.
#[test]
fn a_repeated_append_only_version_is_not_an_overwrite() {
    let (mut engine, _storage) = new_engine();
    engine
        .apply_batch(&batch(vec![upsert_of("submissions", 1, "sub:a", 1)]))
        .expect("first");
    engine
        .apply_batch(&batch(vec![upsert_of("submissions", 2, "sub:a", 1)]))
        .expect("the same version again is a duplicate, not a contract breach");
}

/// Owner drafts and server-authoritative rows are replaced normally.
///
/// The append-only rule is a property of one class, not a global one. If it leaked into the
/// others, every reflection edit would be refused.
#[test]
fn the_other_classes_still_accept_row_replacement() {
    let (mut engine, storage) = new_engine();

    engine
        .apply_batch(&batch(vec![
            upsert_of("reflections", 1, "refl:a", 1),
            upsert_of("grades", 2, "grade:a", 1),
        ]))
        .expect("first versions");
    engine
        .apply_batch(&batch(vec![
            upsert_of("reflections", 3, "refl:a", 2),
            upsert_of("grades", 4, "grade:a", 2),
        ]))
        .expect("replacement is normal for these classes");

    storage.with(|s| {
        assert_eq!(s.row_count(), 2, "replaced, not duplicated");
    });
}

// -------------------------------------------------------------------------------------------
// DoD 6 — one class-conformance test per class, generated from the registry declaration
// -------------------------------------------------------------------------------------------

/// Every registered entity conforms to the policy its class declares.
///
/// Driven from the registry rather than from a hand-written list, so registering a fourth entity
/// extends the coverage automatically — and registering one whose class nobody implemented fails
/// here rather than in production.
#[test]
fn every_registered_class_conforms_to_its_declared_policy() {
    let (engine, _storage) = new_engine();
    let classes: Vec<_> = engine
        .registry()
        .entities()
        .map(|r| (r.entity.clone(), r.conflict_class))
        .collect();

    assert_eq!(classes.len(), 3, "all three classes are exercised");

    for (entity, class) in classes {
        match class {
            // Pull-only: no command may target it.
            ConflictClass::ServerAuthoritative => {
                let (mut e, _s) = new_engine();
                let command_name = command_for(&entity);
                let err = e
                    .enqueue(OutboxEntry::new(named_command(9, &command_name), schema()))
                    .expect_err("server-authoritative entities accept no commands");
                assert!(matches!(
                    err,
                    OutboxError::Registry(RegistryError::ServerAuthoritative { .. })
                ));
            }

            // Commands allowed; a superseded edit is recovered rather than dropped.
            ConflictClass::OwnerDraft => {
                let (mut e, s) = new_engine();
                e.enqueue(OutboxEntry::new(
                    named_command(9, &command_for(&entity)),
                    schema(),
                ))
                .expect("owner drafts accept commands");
                e.apply_results(&push_response(vec![superseded(9)]))
                    .expect("resolves");
                s.with(|st| {
                    assert_eq!(
                        st.recovered.len(),
                        1,
                        "{entity}: a losing edit must survive"
                    );
                });
            }

            // Commands allowed; an existing entry is never replaced.
            ConflictClass::AppendOnly => {
                let (mut e, _s) = new_engine();
                e.enqueue(OutboxEntry::new(
                    named_command(9, &command_for(&entity)),
                    schema(),
                ))
                .expect("append-only streams accept commands");

                e.apply_batch(&batch(vec![upsert_of(entity.as_str(), 1, "x:1", 1)]))
                    .expect("first entry");
                assert!(
                    e.apply_batch(&batch(vec![upsert_of(entity.as_str(), 2, "x:1", 2)]))
                        .is_err(),
                    "{entity}: an existing append-only entry must never be replaced"
                );
            } // Deliberately exhaustive, with no catch-all arm. A fourth conflict class would fail
              // to compile here rather than silently acquiring no conformance case at all — which
              // is what DoD 6 means by generating the tests from the registry declaration.
        }
    }
}

/// The command registered against an entity, for the conformance loop.
fn command_for(entity: &EntityName) -> String {
    match entity.as_str() {
        "grades" => "amend_grade".to_owned(),
        "reflections" => "submit_reflection".to_owned(),
        "submissions" => "add_submission".to_owned(),
        other => panic!("no command registered for {other}"),
    }
}
