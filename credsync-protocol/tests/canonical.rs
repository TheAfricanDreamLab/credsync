//! DoD 2: canonical encoding is byte-stable — the same value always encodes to identical bytes.
//!
//! This is the property the whole integrity layer rests on. Batch checksums and scope digests
//! are computed over this encoding (`docs/spec.md` §5), so if encoding were unstable, two sides
//! holding identical state would compute different digests and the client would declare silent
//! divergence that had not occurred — self-healing against a phantom.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_protocol::canonical;
use proptest::prelude::*;
use serde_json::{Map, Value, json};

proptest! {
    /// Encoding the same value repeatedly yields identical bytes.
    #[test]
    fn encoding_is_deterministic(value in common::batch()) {
        let a = canonical::to_vec(&value).expect("encodes");
        let b = canonical::to_vec(&value).expect("encodes");
        let c = canonical::to_vec(&value).expect("encodes");
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&b, &c);
    }

    /// Object keys come out sorted at every level of nesting.
    #[test]
    fn keys_sort_recursively(doc in common::json_object()) {
        let bytes = canonical::canonicalize(&doc).expect("encodes");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert_keys_sorted(&text);
    }

    /// The encoding carries no incidental whitespace.
    #[test]
    fn no_incidental_whitespace(value in common::push_request()) {
        let bytes = canonical::to_vec(&value).expect("encodes");
        let text = String::from_utf8(bytes).expect("utf-8");
        // Whitespace inside string literals is legitimate; whitespace between tokens is not.
        let outside_strings = strip_string_literals(&text);
        prop_assert!(
            !outside_strings.contains([' ', '\n', '\t', '\r']),
            "found structural whitespace in {outside_strings}"
        );
    }

    /// Re-encoding an opaque host document is idempotent.
    ///
    /// This is the property the digest depends on, and it is why floats are refused inside host
    /// documents (D-028): with a float present this test fails, because `serde_json` parsing is
    /// not exact and the value shifts by one ULP across the cycle.
    #[test]
    fn host_documents_re_encode_identically(doc in common::json_value()) {
        let once = canonical::canonicalize(&doc).expect("encodes");
        let parsed: Value = serde_json::from_slice(&once).expect("decodes");
        let twice = canonical::canonicalize(&parsed).expect("re-encodes");
        prop_assert_eq!(once, twice);
    }
}

/// Key insertion order must not affect output — the property that makes digests comparable
/// between two machines that built the same document differently.
#[test]
fn key_insertion_order_does_not_affect_output() {
    let mut forward = Map::new();
    forward.insert("alpha".into(), json!(1));
    forward.insert("beta".into(), json!(2));
    forward.insert("gamma".into(), json!(3));

    let mut backward = Map::new();
    backward.insert("gamma".into(), json!(3));
    backward.insert("beta".into(), json!(2));
    backward.insert("alpha".into(), json!(1));

    let a = canonical::canonicalize(&Value::Object(forward)).expect("encodes");
    let b = canonical::canonicalize(&Value::Object(backward)).expect("encodes");
    assert_eq!(a, b, "insertion order changed the canonical encoding");
}

/// Struct field declaration order must not affect output.
///
/// This is the reason encoding routes through `serde_json::Value` instead of serializing structs
/// directly: `serde` emits fields in declaration order, so a direct encode would make every
/// digest depend on how a struct happens to be written. Someone tidying a struct months from now
/// would silently change every digest in the system.
#[test]
fn struct_field_order_does_not_affect_output() {
    #[derive(serde::Serialize)]
    struct Forward {
        alpha: u8,
        beta: u8,
    }
    #[derive(serde::Serialize)]
    struct Backward {
        beta: u8,
        alpha: u8,
    }

    let a = canonical::to_vec(&Forward { alpha: 1, beta: 2 }).expect("encodes");
    let b = canonical::to_vec(&Backward { beta: 2, alpha: 1 }).expect("encodes");
    assert_eq!(
        String::from_utf8_lossy(&a),
        String::from_utf8_lossy(&b),
        "declaration order leaked into the canonical encoding"
    );
}

/// Every number the protocol defines is an integer, which has exactly one JSON form.
#[test]
fn protocol_numbers_have_one_representation() {
    let bytes = canonical::to_vec(&credsync_protocol::Seq::new(42).expect("valid")).expect("enc");
    assert_eq!(String::from_utf8_lossy(&bytes), "42");
}

/// Asserts that every object in a JSON document has its keys in sorted order.
fn assert_keys_sorted(text: &str) {
    let value: Value = serde_json::from_str(text).expect("valid json");
    fn walk(v: &Value) {
        match v {
            Value::Object(map) => {
                let keys: Vec<_> = map.keys().cloned().collect();
                let mut sorted = keys.clone();
                sorted.sort();
                assert_eq!(keys, sorted, "object keys are not sorted");
                for child in map.values() {
                    walk(child);
                }
            }
            Value::Array(items) => items.iter().for_each(walk),
            _ => {}
        }
    }
    walk(&value);
}

/// Removes string literals so structural whitespace can be checked in isolation.
fn strip_string_literals(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
                out.push(ch);
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Floats are refused inside host documents, and this is why.
///
/// `serde_json` parsing is not exact: `2.2283095771495367e-21` has bit pattern `..f938`, but
/// re-parsing its own serialization yields `..f939` — one ULP away — which then re-encodes as
/// the shorter `2.228309577149537e-21`. A document containing that float therefore does not
/// survive a serialize/parse/serialize cycle unchanged.
///
/// Because scope digests are computed over the canonical encoding, allowing floats would let two
/// sides holding the same logical row compute different digests, and the client would re-bootstrap
/// against a divergence that never happened.
#[test]
fn floats_are_refused_in_host_documents() {
    use credsync_protocol::{Payload, ProtocolError, Snapshot};

    let doc = common::float_document();
    assert!(matches!(
        Snapshot::new(doc.clone()),
        Err(ProtocolError::FloatNotAllowed { .. })
    ));
    assert!(matches!(
        Payload::new(doc),
        Err(ProtocolError::FloatNotAllowed { .. })
    ));

    // Nested floats are caught too, at any depth and inside arrays.
    let nested = json!({ "a": { "b": [1, 2, { "c": 0.5 }] } });
    assert!(matches!(
        Snapshot::new(nested),
        Err(ProtocolError::FloatNotAllowed { .. })
    ));

    // Integers remain fine - including the full i64 range hosts actually use.
    let ints = json!({ "n": -9223372036854775808i64, "m": 9223372036854775807i64, "z": 0 });
    assert!(Snapshot::new(ints).is_ok());
}

/// The exact instability that motivated the rule, pinned so a future serde_json change is noticed.
#[test]
fn serde_json_float_parsing_is_not_exact() {
    let x: f64 = 2.2283095771495367e-21;
    let once = serde_json::to_string(&x).unwrap();
    let back: f64 = serde_json::from_str(&once).unwrap();
    assert_ne!(
        x.to_bits(),
        back.to_bits(),
        "serde_json float round-trip became exact - D-028 can be revisited"
    );
}
