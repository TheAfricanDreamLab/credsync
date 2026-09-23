//! CS-5: golden fixtures — the committed bytes every wire type must still produce.
//!
//! `tests/roundtrip.rs` proves that no valid value is lost or altered by a trip through the
//! codec. It cannot prove that the bytes are the *right* bytes: rename a field in both the
//! encoder and the decoder and every round-trip test stays green while the wire quietly moves
//! underneath two peers who no longer agree.
//!
//! These fixtures close that gap. Each file in `tests/fixtures/` is the exact canonical encoding
//! of a known value, committed to the repository, and each is asserted in both directions:
//!
//! 1. the committed bytes decode to the expected value — the **decoder** is pinned;
//! 2. re-encoding the expected value reproduces the committed bytes — the **encoder** is pinned.
//!
//! Neither check alone is enough. Together they mean a wire change cannot land silently: it
//! shows up as a fixture diff, which is a reviewable artefact rather than a green test run.
//!
//! # These files are evidence, not output
//!
//! A failing fixture means the wire format moved. The response is to confirm that was intended
//! and that `docs/spec.md` moved with it **in the same pull request** — never to regenerate the
//! file until the test goes quiet. That is why no regeneration script is committed alongside
//! them: a fixture that is one command away from agreeing with whatever the code now does is not
//! a fixture. See `.claude/skills/credsync-protocol/SKILL.md`, "Testing the wire".
//!
//! # What these fixtures do not pin
//!
//! The `checksum` and `digest` values here are fixed literals, not computed. What a fixture pins
//! about them is the field's name, presence, and exact 32-character lowercase-hex shape. The
//! algorithms themselves are pinned by the frozen vectors in `tests/integrity.rs`, which check
//! xxh3 and BLAKE3 against those algorithms' own published outputs.
//!
//! # No network, no clock, no filesystem
//!
//! Fixture bytes are embedded with `include_bytes!` at compile time, so the assertions run on a
//! clean checkout with nothing available but the binary. `cargo` tracks each embedded file, so
//! editing one still triggers a rebuild.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::{
    Batch, BootstrapRequest, BootstrapResponse, BootstrapRow, Change, Command, CommandId,
    CommandName, CommandResult, ConflictClass, Cursor, EntityId, EntityName, EntityRegistration,
    ForcedUpgrade, HexString, LimitBytes, Op, Payload, ProtocolVersion, PullRequest, PullResponse,
    PushRequest, PushResponse, Reason, RowVersion, SchemaVersion, ScopeCursor, ScopeId, Seq,
    Snapshot, Status, canonical,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt::Debug;

/// Asserts a fixture in both directions, then proves the assertion would notice a change.
///
/// The third step is the one that makes the first two worth trusting. CLAUDE.md §4: never assert
/// a property you have not seen fail. Rather than breaking a fixture by hand once and taking it
/// on faith afterwards, every byte of every fixture is perturbed on every run, and the check must
/// reject all of them. A fixture assertion that has somehow become vacuous — compared against
/// itself, or against a type that ignores the field that moved — fails here rather than passing
/// quietly for months.
fn golden<T>(file: &str, bytes: &[u8], expected: &T)
where
    T: DeserializeOwned + Serialize + PartialEq + Debug,
{
    // The file is the bytes. A trailing newline would make it something else that merely looks
    // the same in an editor, and would then be silently tolerated by every later comparison.
    assert_ne!(
        bytes.last(),
        Some(&b'\n'),
        "fixture {file} ends with a newline; fixtures are exact wire bytes, \
         so the file must end with the last byte of the encoding"
    );

    let decoded: T = canonical::from_slice(bytes).unwrap_or_else(|e| {
        panic!(
            "fixture {file} no longer decodes: {e}\n\
             The wire format moved. Confirm that was intended and that docs/spec.md moved with \
             it in this same PR — do not regenerate the fixture to make this pass."
        )
    });
    assert_eq!(
        &decoded, expected,
        "fixture {file} decodes to a different value than expected.\n\
         The decoder's reading of these bytes changed. Check docs/spec.md §2-§3."
    );

    let reencoded = canonical::to_vec(expected).expect("expected value encodes");
    assert_eq!(
        String::from_utf8_lossy(&reencoded),
        String::from_utf8_lossy(bytes),
        "fixture {file} is no longer what this value encodes to.\n\
         The encoder moved: a field was renamed, reordered, added, or dropped. \
         Update docs/spec.md and the fixture together, in one PR — never the fixture alone."
    );

    drill(file, bytes, expected);
}

/// The planted-bug drill: every single-byte perturbation must be rejected.
///
/// Flipping the low bit of any byte of canonical JSON always produces either invalid input or a
/// different value — there is no incidental whitespace to absorb a change, and an unknown key is
/// dropped rather than merged, which shifts the decoded value. So if any perturbed copy still
/// decodes to `expected`, the fixture is not actually pinning that byte.
fn drill<T>(file: &str, bytes: &[u8], expected: &T)
where
    T: DeserializeOwned + PartialEq + Debug,
{
    for i in 0..bytes.len() {
        let mut perturbed = bytes.to_vec();
        perturbed[i] ^= 0b0000_0001;
        if canonical::from_slice::<T>(&perturbed).is_ok_and(|v| &v == expected) {
            panic!(
                "fixture {file} still decodes to the expected value with byte {i} \
                 (0x{:02x} -> 0x{:02x}) changed.\n\
                 That byte is not pinned by this assertion, so a wire change there would land \
                 silently.",
                bytes[i], perturbed[i]
            );
        }
    }
}

/// Declares one fixture: a test asserting it, plus its filename for the coverage check.
macro_rules! fixtures {
    ($( $name:ident : $ty:ty = $file:literal => $build:expr ; )+) => {
        $(
            #[test]
            fn $name() {
                golden::<$ty>($file, include_bytes!(concat!("fixtures/", $file)), &$build);
            }
        )+

        /// Every fixture claimed by a test above. Compared against the directory below, so a
        /// file nobody asserts cannot sit there looking like coverage.
        const DECLARED: &[&str] = &[$($file),+];
    };
}

fixtures! {
    // Opaque host documents. Bounded and validated, but not otherwise modelled.
    snapshot_fixture: Snapshot = "snapshot.json" => Snapshot::new(snapshot_value()).expect("valid snapshot");
    payload_fixture: Payload = "payload.json" => Payload::new(payload_value()).expect("valid payload");

    // Enums, all variants at once: each variant's wire spelling is wire format, and a fixture
    // holding only one of them would leave the others free to drift.
    op_fixture: Vec<Op> = "op.json" => vec![Op::Upsert, Op::Delete];
    status_fixture: Vec<Status> = "status.json" =>
        vec![Status::Applied, Status::Rejected, Status::Superseded];
    conflict_class_fixture: Vec<ConflictClass> = "conflict_class.json" => vec![
        ConflictClass::ServerAuthoritative,
        ConflictClass::OwnerDraft,
        ConflictClass::AppendOnly,
    ];

    entity_registration_fixture: EntityRegistration = "entity_registration.json" =>
        EntityRegistration {
            entity: entity(),
            scope: scope(),
            conflict_class: ConflictClass::OwnerDraft,
            schema_version: schema_version(),
        };

    // Both sides of the op/snapshot rule (docs/spec.md §2.1). The delete fixture is the one that
    // pins `snapshot` being *absent* rather than null — a distinction a single fixture would miss.
    change_upsert_fixture: Change = "change_upsert.json" => change_upsert();
    change_delete_fixture: Change = "change_delete.json" => change_delete();

    batch_fixture: Batch = "batch.json" => batch();
    scope_cursor_fixture: ScopeCursor = "scope_cursor.json" => ScopeCursor {
        scope: scope(),
        cursor: Cursor::new(1206).expect("valid cursor"),
    };

    // With and without the optional `limit_bytes`, for the same reason as the two Change files.
    pull_request_fixture: PullRequest = "pull_request.json" => pull_request();
    pull_request_no_limit_fixture: PullRequest = "pull_request_no_limit.json" =>
        PullRequest {
            protocol: protocol(),
            scopes: vec![ScopeCursor { scope: scope(), cursor: Cursor::new(1206).expect("valid cursor") }],
            limit_bytes: None,
        };
    pull_response_fixture: PullResponse = "pull_response.json" => PullResponse {
        protocol: protocol(),
        batches: vec![batch()],
    };

    bootstrap_request_fixture: BootstrapRequest = "bootstrap_request.json" => BootstrapRequest {
        protocol: protocol(),
        scope: scope(),
        after: Cursor::START,
    };
    bootstrap_row_fixture: BootstrapRow = "bootstrap_row.json" => bootstrap_row();
    bootstrap_response_fixture: BootstrapResponse = "bootstrap_response.json" =>
        BootstrapResponse {
            protocol: protocol(),
            scope: scope(),
            rows: vec![bootstrap_row()],
            next_cursor: Cursor::new(1208).expect("valid cursor"),
            has_more: false,
            checksum: hex("c41d8f0b27e6a35914da70bc8e2f6d03"),
            digest: hex("0a7e4411bd903c26ef58d7142b06915c"),
        };

    command_fixture: Command = "command.json" => command();
    push_request_fixture: PushRequest = "push_request.json" => PushRequest {
        protocol: protocol(),
        commands: vec![command()],
    };

    // Both sides of the status/reason rule (docs/spec.md §3.3). The rejected fixture also carries
    // a non-ASCII reason: `Reason` accepts any UTF-8 because it is shown to a person, and the
    // people this platform serves do not write exclusively in ASCII.
    command_result_applied_fixture: CommandResult = "command_result_applied.json" => applied();
    command_result_rejected_fixture: CommandResult = "command_result_rejected.json" => rejected();
    push_response_fixture: PushResponse = "push_response.json" => PushResponse {
        protocol: protocol(),
        results: vec![applied(), rejected()],
    };

    forced_upgrade_fixture: ForcedUpgrade = "forced_upgrade.json" => ForcedUpgrade {
        min_protocol: ProtocolVersion::new(2).expect("valid protocol"),
        current_protocol: ProtocolVersion::new(3).expect("valid protocol"),
        reason: reason(
            "This version of the app can no longer sync. Update to continue; \
             your queued work is safe.",
        ),
    };
}

/// Every `.json` file in `tests/fixtures/` is asserted by a test above.
///
/// Without this, deleting a fixture's test — or misspelling a filename in the macro — leaves an
/// unasserted file sitting in the directory. It reads as coverage to anyone browsing the repo
/// while proving nothing at all.
#[test]
fn every_fixture_file_is_asserted() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("fixtures directory exists")
        .map(|e| {
            e.expect("readable entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n.ends_with(".json"))
        .collect();
    found.sort();

    let mut declared: Vec<String> = DECLARED.iter().map(|s| (*s).to_owned()).collect();
    declared.sort();

    assert_eq!(
        found, declared,
        "the fixtures directory and the declared fixtures disagree.\n\
         A file on disk with no test asserts nothing; a declared file that is missing would not \
         compile. Add the test, or delete the file."
    );
}

/// The scope every fixture belongs to: `(institution, enrolment)`, as `docs/spec.md` §1 describes.
fn scope() -> ScopeId {
    ScopeId::new("inst:adl:enr:2026-cohort").expect("valid scope")
}

fn entity() -> EntityName {
    EntityName::new("reflections").expect("valid entity")
}

fn protocol() -> ProtocolVersion {
    ProtocolVersion::new(1).expect("valid protocol")
}

fn schema_version() -> SchemaVersion {
    SchemaVersion::new(3).expect("valid schema_version")
}

fn hex(s: &str) -> HexString {
    HexString::new(s).expect("valid hex")
}

fn reason(s: &str) -> Reason {
    Reason::new(s).expect("valid reason")
}

fn command_id(s: &str) -> CommandId {
    CommandId::parse(s).expect("valid UUIDv7")
}

/// A reflection row as a host would write it.
///
/// Deliberately mixed: a string, a bool, a null, an array, and two integers — including a
/// millisecond timestamp, which is the shape a host reaches for when it would otherwise have
/// written a float (`docs/spec.md` §2.2 refuses those).
fn snapshot_value() -> Value {
    json!({
        "lesson_id": "lesson:0191f0c2",
        "body": "Wrote up the clinic visit before the bus came.",
        "word_count": 412,
        "submitted_at_ms": 1_756_137_600_000_i64,
        "tags": ["residency", "week-3"],
        "draft": false,
        "reviewed_by": Value::Null,
    })
}

fn payload_value() -> Value {
    json!({
        "reflection_id": "refl:0191f0c3",
        "body": "Wrote up the clinic visit before the bus came.",
        "word_count": 412,
    })
}

fn change_upsert() -> Change {
    Change {
        seq: Seq::new(1207).expect("valid seq"),
        entity: entity(),
        entity_id: EntityId::new("refl:0191f0c3").expect("valid entity_id"),
        op: Op::Upsert,
        snapshot: Some(Snapshot::new(snapshot_value()).expect("valid snapshot")),
        row_version: RowVersion::new(41).expect("valid row_version"),
        schema_version: schema_version(),
    }
}

fn change_delete() -> Change {
    Change {
        seq: Seq::new(1208).expect("valid seq"),
        entity: entity(),
        entity_id: EntityId::new("refl:0191f0c4").expect("valid entity_id"),
        op: Op::Delete,
        snapshot: None,
        row_version: RowVersion::new(42).expect("valid row_version"),
        schema_version: schema_version(),
    }
}

fn batch() -> Batch {
    Batch {
        scope: scope(),
        changes: vec![change_upsert(), change_delete()],
        next_cursor: Cursor::new(1208).expect("valid cursor"),
        has_more: true,
        checksum: hex("3f1a6c9d0e2b48571c83af4d5e60729b"),
        digest: hex("0a7e4411bd903c26ef58d7142b06915c"),
    }
}

fn bootstrap_row() -> BootstrapRow {
    BootstrapRow {
        entity: entity(),
        entity_id: EntityId::new("refl:0191f0c3").expect("valid entity_id"),
        snapshot: Snapshot::new(snapshot_value()).expect("valid snapshot"),
        row_version: RowVersion::new(41).expect("valid row_version"),
        schema_version: schema_version(),
    }
}

fn pull_request() -> PullRequest {
    PullRequest {
        protocol: protocol(),
        scopes: vec![
            ScopeCursor {
                scope: scope(),
                cursor: Cursor::new(1206).expect("valid cursor"),
            },
            ScopeCursor {
                // A second scope that has never synced, so `cursor: 0` appears on the wire.
                scope: ScopeId::new("inst:adl:lib:public").expect("valid scope"),
                cursor: Cursor::START,
            },
        ],
        limit_bytes: Some(LimitBytes::new(100_000).expect("valid limit_bytes")),
    }
}

fn command() -> Command {
    Command {
        id: command_id("0191f0c2-8a3b-7c4d-9e5f-a1b2c3d4e5f6"),
        name: CommandName::new("submit_reflection").expect("valid name"),
        scope: scope(),
        payload: Payload::new(payload_value()).expect("valid payload"),
        client_ts: 1_756_137_600_000,
        checksum: hex("7b2e15c0a94d3f68e0517cab2d94f831"),
    }
}

fn applied() -> CommandResult {
    CommandResult {
        id: command_id("0191f0c2-8a3b-7c4d-9e5f-a1b2c3d4e5f6"),
        status: Status::Applied,
        reason: None,
        server_seq: Some(Seq::new(1207).expect("valid seq")),
    }
}

fn rejected() -> CommandResult {
    CommandResult {
        id: command_id("0191f0c2-8a3b-7d1e-b2a4-f0e1d2c3b4a5"),
        status: Status::Rejected,
        reason: Some(reason("Soumission refusée : la date limite est passée.")),
        server_seq: None,
    }
}
