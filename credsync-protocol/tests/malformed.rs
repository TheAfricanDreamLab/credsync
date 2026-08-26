//! DoD 4: the decoder rejects malformed and truncated input **without panicking**.
//!
//! The network will eventually hand this crate garbage — a flaky proxy, a corrupted flash page,
//! a hostile client. A panic in a wire decoder is a remote crash, so every path below must
//! return an error rather than unwinding.
//!
//! The truncation sweep is the important one: it takes a *valid* encoding of every wire type and
//! cuts it at every possible byte offset, which reliably produces the half-formed inputs that
//! hand-written cases miss.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use credsync_protocol::{
    Batch, BootstrapResponse, Change, Command, CommandResult, PullRequest, PullResponse,
    PushRequest, PushResponse, canonical,
};
use proptest::prelude::*;

/// Decoding must never panic, whatever the bytes.
macro_rules! never_panics_on {
    ($name:ident, $ty:ty, $strategy:expr) => {
        proptest! {
            #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]
            #[test]
            fn $name(bytes in $strategy) {
                // The result is deliberately ignored: what is asserted is that control returns
                // at all. A panic here fails the test by unwinding.
                let _ = canonical::from_slice::<$ty>(&bytes);
            }
        }
    };
}

never_panics_on!(
    arbitrary_bytes_never_panic_pull_response,
    PullResponse,
    proptest::collection::vec(any::<u8>(), 0..512)
);
never_panics_on!(
    arbitrary_bytes_never_panic_push_request,
    PushRequest,
    proptest::collection::vec(any::<u8>(), 0..512)
);
never_panics_on!(
    arbitrary_bytes_never_panic_change,
    Change,
    proptest::collection::vec(any::<u8>(), 0..512)
);
never_panics_on!(
    arbitrary_text_never_panics_batch,
    Batch,
    "\\PC{0,200}".prop_map(String::into_bytes)
);
never_panics_on!(
    arbitrary_text_never_panics_command,
    Command,
    "\\PC{0,200}".prop_map(String::into_bytes)
);

/// Cutting a valid encoding at every byte offset must never panic.
macro_rules! truncation_sweep {
    ($name:ident, $ty:ty, $strategy:expr) => {
        proptest! {
            #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]
            #[test]
            fn $name(value in $strategy) {
                let full = canonical::to_vec(&value).expect("encodes");
                for cut in 0..full.len() {
                    let _ = canonical::from_slice::<$ty>(&full[..cut]);
                }
                // The untruncated bytes must still decode - proving the sweep tested real input
                // rather than something already broken.
                prop_assert!(canonical::from_slice::<$ty>(&full).is_ok());
            }
        }
    };
}

truncation_sweep!(truncated_batch_never_panics, Batch, common::batch());
truncation_sweep!(truncated_command_never_panics, Command, common::command());
truncation_sweep!(
    truncated_pull_response_never_panics,
    PullResponse,
    common::pull_response()
);
truncation_sweep!(
    truncated_push_response_never_panics,
    PushResponse,
    common::push_response()
);
truncation_sweep!(
    truncated_bootstrap_never_panics,
    BootstrapResponse,
    common::bootstrap_response()
);

/// Structurally valid JSON of the wrong shape is refused, not coerced.
#[test]
fn wrong_shapes_are_refused() {
    let cases: &[(&str, &str)] = &[
        ("null", "null for a struct"),
        ("[]", "array for a struct"),
        ("\"text\"", "string for a struct"),
        ("42", "number for a struct"),
        ("{}", "object missing every required field"),
        ("{\"protocol\":1}", "object missing batches"),
        (
            "{\"protocol\":0,\"batches\":[]}",
            "protocol below its minimum",
        ),
        (
            "{\"protocol\":1,\"batches\":[],\"unexpected\":true}",
            "unknown field is tolerated by serde but must still decode",
        ),
    ];
    for (input, what) in cases {
        let result = canonical::from_slice::<PullResponse>(input.as_bytes());
        // The last case is expected to succeed; the rest must fail. Either way, no panic.
        if what.starts_with("unknown field") {
            assert!(result.is_ok(), "{what}: {input}");
        } else {
            assert!(result.is_err(), "accepted {what}: {input}");
        }
    }
}

/// Deeply nested JSON must be refused rather than overflowing the stack.
///
/// `serde_json` has a recursion limit for exactly this reason; this asserts it is in force,
/// because a stack overflow is an abort that no `Result` can catch.
#[test]
fn deep_nesting_is_refused_not_fatal() {
    let depth = 2048;
    let deep = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let result = canonical::from_slice::<PullResponse>(deep.as_bytes());
    assert!(result.is_err(), "deeply nested input was accepted");
}

/// Invalid UTF-8 is an error, not a panic.
#[test]
fn invalid_utf8_is_refused() {
    let bad = [0x7b, 0x22, 0xff, 0xfe, 0x22, 0x7d]; // {"<invalid>"}
    assert!(canonical::from_slice::<Change>(&bad).is_err());
    assert!(canonical::from_slice::<CommandResult>(&bad).is_err());
}

/// Empty input is an error, not a panic.
#[test]
fn empty_input_is_refused() {
    assert!(canonical::from_slice::<PullRequest>(b"").is_err());
    assert!(canonical::from_slice::<Batch>(b"").is_err());
}

/// Numbers outside the range of their Rust type are refused rather than wrapping.
#[test]
fn numeric_overflow_is_refused() {
    // u64::MAX exceeds the seq ceiling of i64::MAX.
    let over = format!("{}", u64::MAX);
    assert!(canonical::from_slice::<credsync_protocol::Seq>(over.as_bytes()).is_err());

    // Beyond u64 entirely.
    assert!(
        canonical::from_slice::<credsync_protocol::Seq>(b"99999999999999999999999999").is_err()
    );

    // Negative where unsigned is required.
    assert!(canonical::from_slice::<credsync_protocol::Seq>(b"-1").is_err());

    // A float where an integer is required.
    assert!(canonical::from_slice::<credsync_protocol::Seq>(b"1.5").is_err());
}
