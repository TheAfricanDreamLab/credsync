//! CS-21: the N−1 window, and the envelope a refused client receives.
//!
//! `docs/spec.md` §7: *"The server speaks N and N−1. Below N−1 it responds `426` with a
//! forced-upgrade envelope the client core understands."*
//!
//! The window is one version wide on purpose. Every version still accepted is a version the tests
//! must cover and the code must branch on, and a wide window is how a protocol becomes impossible
//! to change.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_protocol::ProtocolVersion;
use credsync_server::version::{Negotiated, VersionPolicy};

fn p(n: u16) -> ProtocolVersion {
    ProtocolVersion::new(n).expect("valid protocol version")
}

// -------------------------------------------------------------------------------------------
// DoD: accepts N and N-1, refuses N-2
// -------------------------------------------------------------------------------------------

#[test]
fn the_window_accepts_n_and_n_minus_one_and_refuses_below() {
    let policy = VersionPolicy::window(p(5));

    assert!(policy.negotiate(p(5)).is_accepted(), "N must be accepted");
    assert!(policy.negotiate(p(4)).is_accepted(), "N-1 must be accepted");

    match policy.negotiate(p(3)) {
        Negotiated::Refused(envelope) => {
            assert_eq!(envelope.min_protocol, p(4));
            assert_eq!(envelope.current_protocol, p(5));
            assert!(
                !envelope.reason.as_str().is_empty(),
                "the envelope must carry something a prompt can show"
            );
        }
        Negotiated::Accepted => panic!("N-2 was accepted; the window is wider than one version"),
    }
}

/// Every version below the window is refused, not just N−2.
#[test]
fn every_version_below_the_window_is_refused() {
    let policy = VersionPolicy::window(p(9));
    for v in 1..=8u16 {
        let expected_accept = v == 8;
        assert_eq!(
            policy.negotiate(p(v)).is_accepted(),
            expected_accept,
            "protocol {v} against a window of 8..=9"
        );
    }
}

/// A server on protocol 1 accepts only 1.
///
/// The window floors at the protocol's own minimum rather than trying to accept 0, which is not a
/// version. Worth asserting because it is the configuration the project actually ships today, and
/// an unchecked `current - 1` would underflow straight into it.
#[test]
fn a_server_on_the_first_protocol_accepts_only_that_one() {
    let policy = VersionPolicy::window(p(1));
    assert_eq!(policy.min(), p(1));
    assert!(policy.negotiate(p(1)).is_accepted());
    assert!(matches!(policy.negotiate(p(2)), Negotiated::Refused(_)));
}

// -------------------------------------------------------------------------------------------
// The other direction
// -------------------------------------------------------------------------------------------

/// A client *newer* than the server is refused, and told the server is behind.
///
/// `docs/spec.md` §7 does not spell this out. Accepting it would mean serving a request containing
/// fields this server has never seen and will silently ignore — and silently ignoring part of a
/// request is how a write goes missing while both sides report success.
///
/// The envelope carries the numbers rather than a verdict, so the client can see
/// `current_protocol` is below its own and say "the service is behind" instead of telling a user to
/// update an app that is already newer.
#[test]
fn a_client_newer_than_the_server_is_refused_with_a_different_reason() {
    let policy = VersionPolicy::window(p(5));

    let Negotiated::Refused(ahead) = policy.negotiate(p(6)) else {
        panic!("a client above the server's current version was accepted");
    };
    let Negotiated::Refused(behind) = policy.negotiate(p(3)) else {
        panic!("a client below the window was accepted");
    };

    assert_ne!(
        ahead.reason.as_str(),
        behind.reason.as_str(),
        "a client that is ahead was told to update its app, which is already newer"
    );
    assert!(
        ahead.current_protocol < p(6),
        "the envelope does not let the client see the server is behind"
    );
}

// -------------------------------------------------------------------------------------------
// Configuration
// -------------------------------------------------------------------------------------------

/// A wider window can be set for a slow rollout.
#[test]
fn an_explicit_window_can_be_wider_than_one_version() {
    let policy = VersionPolicy::new(p(5), p(2)).expect("a valid window");
    for v in 2..=5u16 {
        assert!(policy.negotiate(p(v)).is_accepted(), "protocol {v}");
    }
    assert!(matches!(policy.negotiate(p(1)), Negotiated::Refused(_)));
}

/// A window that accepts nothing is refused at construction.
///
/// A server that refuses every client is a configuration mistake, and the cheapest place to find
/// out is at start-up rather than from the first support ticket.
#[test]
fn a_window_with_no_versions_in_it_is_refused() {
    assert!(VersionPolicy::new(p(3), p(4)).is_err());
}
