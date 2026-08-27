//! DoD 3: every field enforces its `spec.md` size limit, and oversize input is **rejected,
//! not truncated**.
//!
//! Truncation is the failure mode this guards against. A truncated `entity_id` is not a refused
//! request — it is a request that silently addresses a *different row*. Every case below asserts
//! both that the value is refused and, where relevant, that the boundary itself is accepted:
//! a limit that rejects the legal maximum is as wrong as one that accepts an illegal value.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_protocol::{
    CommandId, CommandName, Cursor, EntityId, EntityName, HexString, LimitBytes, Payload,
    ProtocolError, ProtocolVersion, Reason, RowVersion, SchemaVersion, ScopeId, Seq, Snapshot,
    canonical, limits,
};
use serde_json::json;

/// Asserts a string type accepts exactly its limit and refuses one byte more.
macro_rules! boundary {
    ($name:ident, $ty:ty, $max:expr, $ch:expr) => {
        #[test]
        fn $name() {
            let at_limit: String = std::iter::repeat_n($ch, $max).collect();
            assert!(
                <$ty>::new(at_limit).is_ok(),
                "the legal maximum was rejected"
            );

            let over: String = std::iter::repeat_n($ch, $max + 1).collect();
            let err = <$ty>::new(over).expect_err("oversize value was accepted");
            assert!(
                matches!(err, ProtocolError::TooLong { .. }),
                "expected TooLong, got {err:?}"
            );
        }
    };
}

boundary!(
    scope_at_and_over_limit,
    ScopeId,
    limits::SCOPE_MAX_BYTES,
    'a'
);
boundary!(
    entity_at_and_over_limit,
    EntityName,
    limits::ENTITY_MAX_BYTES,
    'a'
);
boundary!(
    entity_id_at_and_over_limit,
    EntityId,
    limits::ENTITY_ID_MAX_BYTES,
    'a'
);
boundary!(
    command_name_at_and_over_limit,
    CommandName,
    limits::COMMAND_NAME_MAX_BYTES,
    'a'
);
boundary!(
    reason_at_and_over_limit,
    Reason,
    limits::REASON_MAX_BYTES,
    'a'
);
/// The hex width is exact, not a ceiling: both algorithms are 128-bit (D-031), so 31, 34 and 64
/// characters are all malformed even though two of them are even and all three are valid hex.
///
/// This replaces an earlier ceiling test that used `HEX_MAX_CHARS + 1` — an odd number, so it was
/// refused for odd length while asserting `TooLong`, and passed for the wrong reason.
#[test]
fn hex_width_is_exact() {
    let hex_of = |n: usize| -> String { std::iter::repeat_n('a', n).collect() };

    assert!(
        HexString::new(hex_of(limits::HEX_CHARS)).is_ok(),
        "the declared width was rejected"
    );

    for short in [2usize, 30, 31] {
        assert!(
            matches!(
                HexString::new(hex_of(short)),
                Err(ProtocolError::TooShort { .. })
            ),
            "accepted {short} characters"
        );
    }
    for long in [33usize, 34, 64] {
        assert!(
            HexString::new(hex_of(long)).is_err(),
            "accepted {long} characters"
        );
    }
}

#[test]
fn empty_strings_are_refused() {
    assert!(matches!(ScopeId::new(""), Err(ProtocolError::Empty { .. })));
    assert!(matches!(
        EntityName::new(""),
        Err(ProtocolError::Empty { .. })
    ));
    assert!(matches!(
        HexString::new(""),
        Err(ProtocolError::Empty { .. })
    ));
}

#[test]
fn charsets_are_enforced() {
    // A space would break the scope's use in a query string.
    assert!(matches!(
        ScopeId::new("has space"),
        Err(ProtocolError::BadCharset { .. })
    ));
    // Entity names are lowercase snake_case; an uppercase name is a different string and would
    // break byte-stability if both were accepted as "the same" entity.
    assert!(matches!(
        EntityName::new("Lessons"),
        Err(ProtocolError::BadCharset { .. })
    ));
    // Uppercase hex encodes the same bytes but is a different string, so it would break
    // byte-stability. Checked at the full width, since a short value is refused for length first.
    assert!(matches!(
        HexString::new(format!("{}AA", "a".repeat(30))),
        Err(ProtocolError::BadCharset { .. })
    ));
    // The old odd-length rule is subsumed by the exact width: an odd count cannot equal 32.
    assert!(matches!(
        HexString::new("abc"),
        Err(ProtocolError::TooShort { .. })
    ));
    // Control characters must not reach a log or a terminal.
    assert!(matches!(
        Reason::new("bad\u{7}bell"),
        Err(ProtocolError::BadCharset { .. })
    ));
}

/// A multi-byte character must not smuggle a value past a limit expressed in bytes.
#[test]
fn limits_are_bytes_not_characters() {
    // 'é' is two bytes in UTF-8, so 64 of them are 128 bytes: at the scope limit by bytes,
    // but only 64 characters. It must be judged by bytes.
    let s: String = std::iter::repeat_n('é', limits::SCOPE_MAX_BYTES).collect();
    assert_eq!(s.chars().count(), limits::SCOPE_MAX_BYTES);
    assert!(s.len() > limits::SCOPE_MAX_BYTES);
    // Refused - though for charset here, since 'é' is not in the scope alphabet either.
    assert!(ScopeId::new(s).is_err());

    // A case where only the byte length can decide: printable ASCII is legal in entity_id,
    // so length is the sole constraint.
    let ok: String = std::iter::repeat_n('a', limits::ENTITY_ID_MAX_BYTES).collect();
    assert!(EntityId::new(ok).is_ok());
}

#[test]
fn numeric_ranges_are_enforced() {
    assert!(Seq::new(0).is_err(), "seq is 1-based");
    assert!(Seq::new(limits::SEQ_MAX).is_ok());
    assert!(
        Seq::new(limits::SEQ_MAX + 1).is_err(),
        "seq must not exceed i64::MAX - the log column is a signed bigserial"
    );

    assert!(RowVersion::new(0).is_err());
    assert!(RowVersion::new(limits::SEQ_MAX).is_ok());
    assert!(RowVersion::new(limits::SEQ_MAX + 1).is_err());

    assert!(ProtocolVersion::new(0).is_err());
    assert!(ProtocolVersion::new(1).is_ok());

    assert!(SchemaVersion::new(0).is_err());

    assert!(LimitBytes::new(0).is_err());
    assert!(LimitBytes::new(limits::LIMIT_BYTES_MAX).is_ok());
    assert!(LimitBytes::new(limits::LIMIT_BYTES_MAX + 1).is_err());
}

/// Zero is legal for a cursor and nowhere else. Conflating the two is how an off-by-one
/// silently becomes a skipped change.
#[test]
fn cursor_permits_zero_but_seq_does_not() {
    assert!(Cursor::new(0).is_ok());
    assert_eq!(Cursor::START.get(), 0);
    assert!(Seq::new(0).is_err());
}

#[test]
fn documents_must_be_objects() {
    for not_object in [
        json!(1),
        json!("s"),
        json!([1, 2]),
        json!(null),
        json!(true),
    ] {
        assert!(
            matches!(
                Snapshot::new(not_object.clone()),
                Err(ProtocolError::NotAnObject { .. })
            ),
            "accepted a non-object: {not_object}"
        );
    }
    assert!(Snapshot::new(json!({})).is_ok());
}

#[test]
fn document_at_exactly_the_limit_is_accepted() {
    // Search for a filler length whose canonical encoding is exactly PAYLOAD_MAX_BYTES, then
    // assert it is accepted. Without this the suite only proves "far under" and "far over"
    // behave, never the byte the specification actually names.
    let mut exact = None;
    for extra in 0..64usize {
        let filler = limits::PAYLOAD_MAX_BYTES - 16 + extra;
        let doc = common::document_of_size(filler);
        if canonical::canonicalize(&doc).expect("encodes").len() == limits::PAYLOAD_MAX_BYTES {
            exact = Some(doc);
            break;
        }
    }
    let doc = exact.expect("a document of exactly the limit should be constructible");
    assert_eq!(
        canonical::canonicalize(&doc).expect("encodes").len(),
        limits::PAYLOAD_MAX_BYTES
    );
    assert!(
        Payload::new(doc).is_ok(),
        "a payload of exactly PAYLOAD_MAX_BYTES was rejected"
    );
}

#[test]
fn document_size_limits_are_enforced_over_canonical_bytes() {
    // Comfortably under.
    assert!(Payload::new(common::document_of_size(1024)).is_ok());

    // Comfortably over: the filler alone exceeds the limit.
    let over = common::document_of_size(limits::PAYLOAD_MAX_BYTES + 64);
    let err = Payload::new(over).expect_err("oversize payload accepted");
    assert!(matches!(err, ProtocolError::TooLong { .. }), "{err:?}");

    let over_snap = common::document_of_size(limits::SNAPSHOT_MAX_BYTES + 64);
    assert!(matches!(
        Snapshot::new(over_snap),
        Err(ProtocolError::TooLong { .. })
    ));
}

#[test]
fn command_ids_must_be_lowercase_uuid_v7() {
    // Version nibble is 7.
    assert!(CommandId::parse("0190f8c1-2a3b-7c4d-8e5f-60718293a4b5").is_ok());

    // Version 4 is a valid UUID but not what this protocol accepts.
    let err = CommandId::parse("0190f8c1-2a3b-4c4d-8e5f-60718293a4b5")
        .expect_err("accepted a non-v7 uuid");
    assert!(
        matches!(err, ProtocolError::NotUuidV7 { found: 4 }),
        "{err:?}"
    );

    for bad in [
        "",
        "not-a-uuid",
        "0190F8C1-2A3B-7C4D-8E5F-60718293A4B5",  // uppercase
        "0190f8c12a3b7c4d8e5f60718293a4b5",      // unhyphenated
        "0190f8c1-2a3b-7c4d-8e5f-60718293a4b",   // too short
        "0190f8c1-2a3b-7c4d-8e5f-60718293a4b56", // too long
        "0190f8c1_2a3b_7c4d_8e5f_60718293a4b5",  // wrong separator
        "0190f8c1-2a3b-7c4d-8e5f-60718293a4bg",  // non-hex digit
    ] {
        assert!(
            CommandId::parse(bad).is_err(),
            "accepted a malformed command id: {bad:?}"
        );
    }
}

/// Deserialization must apply the same validation as the constructor, or the wire path becomes
/// a way around the limits.
#[test]
fn wire_path_enforces_the_same_limits() {
    let over = "a".repeat(limits::ENTITY_MAX_BYTES + 1);
    let json = format!("\"{over}\"");
    let decoded: Result<EntityName, _> = canonical::from_slice(json.as_bytes());
    assert!(
        decoded.is_err(),
        "an oversize entity name got in through deserialization"
    );

    let bad_seq = canonical::from_slice::<Seq>(b"0");
    assert!(bad_seq.is_err(), "seq 0 got in through deserialization");

    let bad_id = canonical::from_slice::<CommandId>(b"\"nope\"");
    assert!(
        bad_id.is_err(),
        "a bad command id got in through deserialization"
    );
}

/// Both push limits must bind. A count limit alone would permit 256 maximum-size payloads,
/// which is 16 MB on a link chosen because it tolerates 2G.
#[test]
fn push_enforces_count_and_total_bytes() {
    use credsync_protocol::PushRequest;
    use proptest::strategy::{Strategy, ValueTree};
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let one = common::command()
        .new_tree(&mut runner)
        .expect("strategy")
        .current();

    let protocol = ProtocolVersion::new(1).expect("valid");

    // Within both limits.
    let ok = PushRequest {
        protocol,
        commands: vec![one.clone(); 8],
    };
    assert!(ok.validate().is_ok());

    // Over the count limit.
    let too_many = PushRequest {
        protocol,
        commands: vec![one.clone(); limits::COMMANDS_MAX_COUNT + 1],
    };
    assert!(matches!(
        too_many.validate(),
        Err(ProtocolError::TooManyItems { .. })
    ));

    // Under the count limit but over the byte limit: the case a count-only check would miss.
    let big_payload = Payload::new(common::document_of_size(60 * 1024)).expect("valid payload");
    let heavy = credsync_protocol::Command {
        payload: big_payload,
        ..one
    };
    let too_big = PushRequest {
        protocol,
        commands: vec![heavy; 32],
    };
    assert!(
        matches!(too_big.validate(), Err(ProtocolError::TooLong { .. })),
        "32 large commands slipped past the byte limit"
    );
}

/// The scope list is bounded. An unbounded list would let one request fan out into arbitrarily
/// many change-log queries.
#[test]
fn pull_request_bounds_its_scope_list() {
    use credsync_protocol::{PullRequest, ScopeCursor};

    let one = ScopeCursor {
        scope: ScopeId::new("inst:1").expect("valid"),
        cursor: Cursor::START,
    };
    let protocol = ProtocolVersion::new(1).expect("valid");

    let ok = PullRequest {
        protocol,
        scopes: vec![one.clone(); limits::SCOPES_MAX_COUNT],
        limit_bytes: None,
    };
    assert!(ok.validate().is_ok(), "the legal maximum was rejected");

    let over = PullRequest {
        protocol,
        scopes: vec![one; limits::SCOPES_MAX_COUNT + 1],
        limit_bytes: None,
    };
    assert!(matches!(
        over.validate(),
        Err(ProtocolError::TooManyItems { .. })
    ));
}

/// A rejected checksum must not report itself as a `digest`. The same type carries both, so the
/// field name is a parameter - an error naming the wrong field sends a reader to the wrong place.
#[test]
fn hex_errors_name_the_field_they_came_from() {
    let bad_charset = format!("{}z", "a".repeat(31));
    match HexString::new_named("checksum", bad_charset.clone()) {
        Err(ProtocolError::BadCharset { field, .. }) => assert_eq!(field, "checksum"),
        other => panic!("expected BadCharset for checksum, got {other:?}"),
    }
    match HexString::new(bad_charset) {
        Err(ProtocolError::BadCharset { field, .. }) => assert_eq!(field, "digest"),
        other => panic!("expected BadCharset for digest, got {other:?}"),
    }
}

/// `Reason` is shown to a person, so it accepts UTF-8 rather than ASCII alone - the people using
/// this platform do not write exclusively in ASCII. Control characters stay out.
#[test]
fn reason_accepts_utf8_but_not_control_characters() {
    assert!(Reason::new("Ekpenyong's submission arrived late").is_ok());
    assert!(Reason::new("deadline passed \u{2014} see schedule").is_ok());
    assert!(Reason::new("submiss\u{e3}o atrasada").is_ok());
    assert!(Reason::new("\u{431}\u{43e}\u{43b}\u{44c}\u{448}\u{435}").is_ok());

    assert!(matches!(
        Reason::new("bad\u{7}bell"),
        Err(ProtocolError::BadCharset { .. })
    ));
    assert!(matches!(
        Reason::new("line\nbreak"),
        Err(ProtocolError::BadCharset { .. })
    ));

    // The limit is bytes, so a multi-byte reason fits fewer characters.
    let long: String = std::iter::repeat_n('\u{2014}', limits::REASON_MAX_BYTES).collect();
    assert!(long.len() > limits::REASON_MAX_BYTES);
    assert!(matches!(
        Reason::new(long),
        Err(ProtocolError::TooLong { .. })
    ));
}
