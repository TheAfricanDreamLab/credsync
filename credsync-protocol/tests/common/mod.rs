//! Shared proptest strategies for the wire types.
//!
//! Strategies generate values that are *valid by construction*, so a round-trip failure is a
//! codec bug rather than a strategy bug. Invalid values are constructed explicitly in the tests
//! that care about rejection.

// Helpers are shared across test binaries; each one uses a different subset, so both lints
// fire spuriously. `unreachable_pub` matters because CI builds tests with `-D warnings`.
#![allow(dead_code, unreachable_pub)]

use credsync_protocol::{
    Batch, BootstrapResponse, BootstrapRow, Change, Command, CommandId, CommandName, CommandResult,
    ConflictClass, Cursor, EntityId, EntityName, EntityRegistration, ForcedUpgrade, HexString,
    LimitBytes, Op, Payload, ProtocolVersion, PullRequest, PullResponse, PushRequest, PushResponse,
    Reason, RowVersion, SchemaVersion, ScopeCursor, ScopeId, Seq, Snapshot, Status, limits,
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

pub fn scope_id() -> impl Strategy<Value = ScopeId> {
    "[a-z][a-z0-9:_-]{0,40}".prop_filter_map("valid scope", |s| ScopeId::new(s).ok())
}

pub fn entity_name() -> impl Strategy<Value = EntityName> {
    "[a-z][a-z0-9_]{0,30}".prop_filter_map("valid entity", |s| EntityName::new(s).ok())
}

pub fn entity_id() -> impl Strategy<Value = EntityId> {
    "[A-Za-z0-9_:-]{1,40}".prop_filter_map("valid entity_id", |s| EntityId::new(s).ok())
}

pub fn command_name() -> impl Strategy<Value = CommandName> {
    "[a-z][a-z0-9_]{0,30}".prop_filter_map("valid name", |s| CommandName::new(s).ok())
}

pub fn reason() -> impl Strategy<Value = Reason> {
    "[a-zA-Z0-9 .,'-]{1,80}".prop_filter_map("valid reason", |s| Reason::new(s).ok())
}

pub fn hex() -> impl Strategy<Value = HexString> {
    "([0-9a-f]{2}){1,32}".prop_filter_map("valid hex", |s| HexString::new(s).ok())
}

pub fn command_id() -> impl Strategy<Value = CommandId> {
    proptest::array::uniform16(any::<u8>()).prop_map(|mut b| {
        // Force the version nibble to 7 rather than filtering, so the strategy never starves.
        b[6] = (b[6] & 0x0f) | 0x70;
        CommandId::from_bytes(b).unwrap_or_else(|_| unreachable!("version nibble forced to 7"))
    })
}

pub fn seq() -> impl Strategy<Value = Seq> {
    (limits::SEQ_MIN..=limits::SEQ_MAX).prop_filter_map("valid seq", |v| Seq::new(v).ok())
}

pub fn row_version() -> impl Strategy<Value = RowVersion> {
    (limits::SEQ_MIN..=limits::SEQ_MAX)
        .prop_filter_map("valid row_version", |v| RowVersion::new(v).ok())
}

pub fn cursor() -> impl Strategy<Value = Cursor> {
    (0u64..=limits::SEQ_MAX).prop_filter_map("valid cursor", |v| Cursor::new(v).ok())
}

pub fn protocol_version() -> impl Strategy<Value = ProtocolVersion> {
    (1u16..=u16::MAX).prop_filter_map("valid protocol", |v| ProtocolVersion::new(v).ok())
}

pub fn schema_version() -> impl Strategy<Value = SchemaVersion> {
    (1u16..=u16::MAX).prop_filter_map("valid schema_version", |v| SchemaVersion::new(v).ok())
}

pub fn limit_bytes() -> impl Strategy<Value = LimitBytes> {
    (limits::LIMIT_BYTES_MIN..=limits::LIMIT_BYTES_MAX)
        .prop_filter_map("valid limit_bytes", |v| LimitBytes::new(v).ok())
}

pub fn op() -> impl Strategy<Value = Op> {
    prop_oneof![Just(Op::Upsert), Just(Op::Delete)]
}

pub fn status() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Applied),
        Just(Status::Rejected),
        Just(Status::Superseded)
    ]
}

pub fn conflict_class() -> impl Strategy<Value = ConflictClass> {
    prop_oneof![
        Just(ConflictClass::ServerAuthoritative),
        Just(ConflictClass::OwnerDraft),
        Just(ConflictClass::AppendOnly),
    ]
}

/// Arbitrary JSON, including the nesting and mixed types real host documents contain.
pub fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i32>().prop_map(Value::from),
        // Deliberately no floats: they are refused inside host documents because they break
        // canonical encoding (DECISIONS.md D-028). `float_document()` below generates one for
        // the test that asserts the refusal.
        any::<i64>().prop_map(Value::from),
        "[a-zA-Z0-9 _-]{0,20}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::from),
            proptest::collection::hash_map("[a-z]{1,8}", inner, 0..4)
                .prop_map(|m| { Value::Object(m.into_iter().collect::<Map<String, Value>>()) }),
        ]
    })
}

pub fn json_object() -> impl Strategy<Value = Value> {
    proptest::collection::hash_map("[a-z][a-z0-9_]{0,10}", json_value(), 0..5)
        .prop_map(|m| Value::Object(m.into_iter().collect::<Map<String, Value>>()))
}

pub fn snapshot() -> impl Strategy<Value = Snapshot> {
    json_object().prop_filter_map("within size", |v| Snapshot::new(v).ok())
}

pub fn payload() -> impl Strategy<Value = Payload> {
    json_object().prop_filter_map("within size", |v| Payload::new(v).ok())
}

pub fn change() -> impl Strategy<Value = Change> {
    (
        seq(),
        entity_name(),
        entity_id(),
        op(),
        snapshot(),
        row_version(),
        schema_version(),
    )
        .prop_map(
            |(seq, entity, entity_id, op, snap, row_version, schema_version)| Change {
                seq,
                entity,
                entity_id,
                op,
                // Keep the op/snapshot rule satisfied: a tombstone carries no snapshot.
                snapshot: match op {
                    Op::Upsert => Some(snap),
                    Op::Delete => None,
                },
                row_version,
                schema_version,
            },
        )
}

pub fn batch() -> impl Strategy<Value = Batch> {
    (
        scope_id(),
        proptest::collection::vec(change(), 0..4),
        cursor(),
        any::<bool>(),
        hex(),
        hex(),
    )
        .prop_map(
            |(scope, mut changes, next_cursor, has_more, checksum, digest)| {
                // Strictly seq-ordered within a scope, per spec.md section 4.
                changes.sort_by_key(|c| c.seq.get());
                changes.dedup_by_key(|c| c.seq.get());
                Batch {
                    scope,
                    changes,
                    next_cursor,
                    has_more,
                    checksum,
                    digest,
                }
            },
        )
}

pub fn command() -> impl Strategy<Value = Command> {
    (
        command_id(),
        command_name(),
        scope_id(),
        payload(),
        any::<i64>(),
        hex(),
    )
        .prop_map(|(id, name, scope, payload, client_ts, checksum)| Command {
            id,
            name,
            scope,
            payload,
            client_ts,
            checksum,
        })
}

pub fn command_result() -> impl Strategy<Value = CommandResult> {
    (command_id(), status(), reason(), seq()).prop_map(|(id, status, reason, server_seq)| {
        CommandResult {
            id,
            status,
            // A rejection must carry a reason; anything else may omit both.
            reason: matches!(status, Status::Rejected).then_some(reason),
            server_seq: matches!(status, Status::Applied).then_some(server_seq),
        }
    })
}

pub fn pull_request() -> impl Strategy<Value = PullRequest> {
    (
        protocol_version(),
        proptest::collection::vec(
            (scope_id(), cursor()).prop_map(|(scope, cursor)| ScopeCursor { scope, cursor }),
            0..4,
        ),
        proptest::option::of(limit_bytes()),
    )
        .prop_map(|(protocol, scopes, limit_bytes)| PullRequest {
            protocol,
            scopes,
            limit_bytes,
        })
}

pub fn pull_response() -> impl Strategy<Value = PullResponse> {
    (protocol_version(), proptest::collection::vec(batch(), 0..3))
        .prop_map(|(protocol, batches)| PullResponse { protocol, batches })
}

pub fn push_request() -> impl Strategy<Value = PushRequest> {
    (
        protocol_version(),
        proptest::collection::vec(command(), 0..4),
    )
        .prop_map(|(protocol, commands)| PushRequest { protocol, commands })
}

pub fn push_response() -> impl Strategy<Value = PushResponse> {
    (
        protocol_version(),
        proptest::collection::vec(command_result(), 0..4),
    )
        .prop_map(|(protocol, results)| PushResponse { protocol, results })
}

pub fn bootstrap_row() -> impl Strategy<Value = BootstrapRow> {
    (
        entity_name(),
        entity_id(),
        snapshot(),
        row_version(),
        schema_version(),
    )
        .prop_map(
            |(entity, entity_id, snapshot, row_version, schema_version)| BootstrapRow {
                entity,
                entity_id,
                snapshot,
                row_version,
                schema_version,
            },
        )
}

pub fn bootstrap_response() -> impl Strategy<Value = BootstrapResponse> {
    (
        protocol_version(),
        scope_id(),
        proptest::collection::vec(bootstrap_row(), 0..3),
        cursor(),
        any::<bool>(),
        hex(),
        hex(),
    )
        .prop_map(
            |(protocol, scope, rows, next_cursor, has_more, checksum, digest)| BootstrapResponse {
                protocol,
                scope,
                rows,
                next_cursor,
                has_more,
                checksum,
                digest,
            },
        )
}

pub fn entity_registration() -> impl Strategy<Value = EntityRegistration> {
    (
        entity_name(),
        scope_id(),
        conflict_class(),
        schema_version(),
    )
        .prop_map(
            |(entity, scope, conflict_class, schema_version)| EntityRegistration {
                entity,
                scope,
                conflict_class,
                schema_version,
            },
        )
}

pub fn forced_upgrade() -> impl Strategy<Value = ForcedUpgrade> {
    (protocol_version(), protocol_version(), reason()).prop_map(
        |(min_protocol, current_protocol, reason)| ForcedUpgrade {
            min_protocol,
            current_protocol,
            reason,
        },
    )
}

/// A document containing a float, for asserting that floats are refused.
pub fn float_document() -> Value {
    json!({ "score": 2.2283095771495367e-21 })
}

/// A JSON document of roughly `target` canonical bytes, for testing size limits.
pub fn document_of_size(target: usize) -> Value {
    let filler = "x".repeat(target);
    json!({ "f": filler })
}
