//! Protocol version negotiation, and the `426` envelope. `docs/spec.md` §7.
//!
//! # The server speaks N and N−1
//!
//! One version of grace. A client on the previous protocol keeps working while its update rolls
//! out; a client below that is told to upgrade, in a form its core understands rather than as an
//! HTTP error it has to guess at.
//!
//! The window is deliberately narrow. Every version the server still accepts is a version its
//! tests must cover and its code must branch on, and a wide window is how a protocol becomes
//! impossible to change. One step is enough for an app update to reach devices that are switched
//! on; a device switched off for longer is already going to need an update.
//!
//! # A refusal is not an error
//!
//! [`Negotiated::Refused`] carries a [`ForcedUpgrade`], not a failure. The server is answering
//! correctly — it understood the request well enough to know it cannot serve it, and it is telling
//! the client exactly what would work. Modelling that as an error would push it into the transport
//! layer's error handling, where the one thing a client must do in response (stop pushing, keep its
//! outbox, prompt the user) has nowhere to live.
//!
//! # The client must never drop what it holds
//!
//! `docs/spec.md` §7: *"The client then queues its outbox and surfaces an upgrade prompt — it never
//! drops queued work."* That half lives in `credsync-core`; this module's job is to produce an
//! envelope precise enough for the client to act on, which means carrying **both** the minimum and
//! the current version rather than just "no".

use credsync_protocol::{ForcedUpgrade, ProtocolVersion, Reason};

/// What the server decided about a request's protocol version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Negotiated {
    /// The version is inside the window; serve the request.
    Accepted,

    /// Outside the window. The envelope tells the client what would work.
    ///
    /// Not an error — see the module docs.
    Refused(ForcedUpgrade),
}

impl Negotiated {
    /// Whether the request may be served.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// The versions a server accepts.
///
/// Constructed once at start-up. Holding `min` explicitly rather than deriving it on every request
/// means an operator can widen the window during a slow rollout without a code change, and that the
/// window a server is actually running is a value that can be logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionPolicy {
    current: ProtocolVersion,
    min: ProtocolVersion,
}

impl VersionPolicy {
    /// The standard window: `current` and one below it.
    ///
    /// Floors at the protocol's own minimum, so a server running protocol 1 accepts only 1 rather
    /// than trying to accept 0 — which is not a version.
    #[must_use]
    pub fn window(current: ProtocolVersion) -> Self {
        let min = ProtocolVersion::new(current.get().saturating_sub(1)).unwrap_or(current);
        Self { current, min }
    }

    /// A window with an explicit minimum, for a rollout that needs more grace than one version.
    ///
    /// # Errors
    /// Returns `Err` if `min` is above `current`, which would accept nothing at all — a server that
    /// refuses every client is a configuration mistake worth refusing to start on rather than
    /// discovering from the first support ticket.
    pub const fn new(current: ProtocolVersion, min: ProtocolVersion) -> Result<Self, &'static str> {
        if min.get() > current.get() {
            return Err("the minimum protocol version is above the current one");
        }
        Ok(Self { current, min })
    }

    /// The newest version this server speaks.
    #[must_use]
    pub const fn current(&self) -> ProtocolVersion {
        self.current
    }

    /// The oldest version this server still accepts.
    #[must_use]
    pub const fn min(&self) -> ProtocolVersion {
        self.min
    }

    /// Decides whether a request's protocol version can be served.
    ///
    /// # Both directions are refused, and the envelope says which
    ///
    /// Below `min` is the case `docs/spec.md` §7 describes: the client is behind and should update.
    ///
    /// **Above `current` is refused too**, which the spec does not spell out. A client speaking a
    /// protocol this server has never seen may be sending fields it will silently ignore, and
    /// silently ignoring part of a request is how a write goes missing while both sides report
    /// success. Refusing is the honest answer.
    ///
    /// The same envelope serves both, because it carries the numbers rather than a verdict: a
    /// client whose own version exceeds `current_protocol` can see that the *server* is behind and
    /// say so, instead of telling a user to update an app that is already newer than the service.
    #[must_use]
    pub fn negotiate(&self, client: ProtocolVersion) -> Negotiated {
        if client.get() >= self.min.get() && client.get() <= self.current.get() {
            return Negotiated::Accepted;
        }
        Negotiated::Refused(self.envelope(client))
    }

    /// Builds the `426` envelope for a client outside the window.
    fn envelope(&self, client: ProtocolVersion) -> ForcedUpgrade {
        // Two different situations, two different sentences. A client told to "update the app" when
        // the app is already newer than the server would send its user somewhere useless.
        let reason = if client.get() > self.current.get() {
            "This service is running an older version than this app. Please try again later."
        } else {
            "This app is too old to sync. Please update it to continue."
        };

        ForcedUpgrade {
            min_protocol: self.min,
            current_protocol: self.current,
            reason: Reason::new(reason)
                .unwrap_or_else(|_| unreachable!("literal is a valid reason")),
        }
    }
}
