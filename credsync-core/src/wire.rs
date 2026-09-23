//! What the engine asks to be sent, and what comes back.
//!
//! These wrap the request and response shapes from `credsync-protocol` rather than redefining
//! them. The protocol crate owns the wire; this crate owns *when* to put something on it. Two
//! definitions of a pull request would be two things to keep in step, and `docs/spec.md` would
//! only be law over one of them.

use credsync_protocol::{
    BootstrapRequest, BootstrapResponse, ForcedUpgrade, PullRequest, PullResponse, PushRequest,
    PushResponse,
};

/// A request the engine wants performed.
///
/// `docs/spec.md` §4: **push precedes pull in every cycle**, so a client immediately observes the
/// server's transformation of its own writes. That ordering is the engine's to enforce; the
/// transport performs what it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireRequest {
    /// `GET /sync/bootstrap` — first sync for a scope, or a re-bootstrap after divergence.
    Bootstrap(BootstrapRequest),
    /// `GET /sync/pull` — walk the change log forward from the persisted cursor.
    Pull(PullRequest),
    /// `POST /sync/push` — submit queued commands.
    Push(PushRequest),
}

/// A decoded, checksum-verified response.
///
/// A value of this type means the bytes parsed and their checksum matched. Corruption is a
/// [`TransportError::Malformed`](crate::TransportError::Malformed) instead, so the engine's
/// response handling is never written against a batch that might be garbage.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WireResponse {
    /// Rows for a scope being bootstrapped.
    Bootstrap(BootstrapResponse),
    /// One batch per requested scope.
    Pull(PullResponse),
    /// One result per submitted command.
    Push(PushResponse),
    /// The `426` envelope: this client is below the server's N-1 window.
    ///
    /// A response, not an error, because it is the server answering correctly — and because
    /// `docs/spec.md` §7 requires a specific behaviour of the engine when it arrives: **queue the
    /// outbox, never drop it**, and surface an upgrade prompt. Filing it under "transport
    /// failure" invites exactly the discard this protocol exists to prevent.
    UpgradeRequired(ForcedUpgrade),
}
