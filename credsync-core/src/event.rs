//! What the outside world tells the engine.
//!
//! Events are the *only* way information enters. Nothing is read ambiently — not the time, not
//! connectivity, not whether a request came back. That is what makes a run replayable: the same
//! event sequence produces the same effects, forever, on any machine.
//!
//! Storage does not appear here. `Storage::transact` answers immediately, so its result is a
//! return value rather than an event (D-037). Transport does appear, because a request handed to
//! the network is answered later or never — and "never" is the common case on the links credSync
//! is built for.

use crate::error::TransportError;
use crate::types::{RequestId, Timestamp};
use crate::wire::WireResponse;

/// Something that happened, told to the engine.
///
/// `#[non_exhaustive]`: later slices add variants — the outbox at CS-8 (#9), forced upgrade at
/// CS-21 (#22). Callers match with a catch-all arm; the engine's own `match` never does, so a new
/// variant is a compile error inside this crate and a no-op outside it. That asymmetry is
/// deliberate: a forgotten transition here is a lost write, and the compiler is the only reviewer
/// that never gets tired.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// Time has passed, and here is what time it is now.
    ///
    /// Drives retry deadlines and the periodic backstop. The engine never asks what time it is in
    /// order to decide whether a deadline has passed — it is told, so a simulated run can advance
    /// a year in a millisecond and a real device in an aeroplane can advance not at all.
    Tick {
        /// The current time, per the caller's clock.
        now: Timestamp,
    },

    /// The application wants a sync cycle.
    ///
    /// `docs/spec.md` §4 names the triggers: app foreground, connectivity regained, silent push
    /// hint, periodic backstop. All four arrive here, undistinguished, because the engine's
    /// response to them is identical — and a push notification is *only ever a hint to run the
    /// loop*, never a carrier of state.
    SyncRequested,

    /// Connectivity changed.
    ///
    /// A hint, not a fact. The engine still attempts requests when told it is offline, because
    /// every platform's reachability API lies in both directions — captive portals report online,
    /// and a working connection sometimes reports offline. What this genuinely buys is not
    /// starting a retry storm the moment a radio drops.
    ConnectivityChanged {
        /// What the platform currently believes.
        online: bool,
    },

    /// A request finished, one way or another.
    TransportResponse {
        /// The handle [`Transport::enqueue`](crate::Transport::enqueue) returned.
        ///
        /// Responses are correlated by this and never by arrival order. The links this engine
        /// assumes reorder and duplicate freely, so "the next response is the answer to the last
        /// request" is false often enough to corrupt state.
        id: RequestId,
        /// The decoded response, or why there isn't one.
        result: Result<WireResponse, TransportError>,
    },
}
