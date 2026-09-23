//! The small value types the core's surface is written in terms of.

use core::fmt;

/// A point in time, Unix milliseconds.
///
/// The core never obtains one of these by asking the world — it receives them, either through
/// [`Event::Tick`](crate::Event::Tick) or from [`Clock::now`](crate::Clock::now) on an injected
/// clock. That is the whole point: a timestamp is data flowing in, never an ambient read, so a
/// seeded run replays identically forever.
///
/// Signed, and deliberately not clamped to be positive. `client_ts` on the wire is `i64`
/// (`docs/spec.md` §2.1), device clocks are wrong often and wrong badly, and a device insisting
/// it is 1969 must be representable so the engine can decline to trust it. Refusing to model a
/// bad clock does not make the bad clock go away; it just moves the failure somewhere less
/// convenient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Wraps a Unix-millisecond value.
    #[must_use]
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// The underlying Unix-millisecond value.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// This instant advanced by `ms` milliseconds, saturating at the ends of the range.
    ///
    /// Saturating rather than wrapping: a retry scheduled by overflow would land in the distant
    /// past and fire immediately, turning a backoff into a hot loop against a server that is
    /// already struggling.
    #[must_use]
    pub const fn saturating_add_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }

    /// Milliseconds from `self` to `later`, saturating rather than overflowing.
    #[must_use]
    pub const fn millis_until(self, later: Self) -> i64 {
        later.0.saturating_sub(self.0)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

/// A handle identifying one in-flight request.
///
/// Minted by [`Transport::enqueue`](crate::Transport::enqueue) and quoted back by
/// [`Event::TransportResponse`](crate::Event::TransportResponse), so the engine can match a
/// response to the request that caused it. On a link that reorders and duplicates — which is the
/// only kind of link this project assumes — correlating by arrival order would be wrong roughly
/// as often as the network is bad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// Wraps a transport-assigned handle.
    #[must_use]
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// The underlying handle.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "req#{}", self.0)
    }
}
