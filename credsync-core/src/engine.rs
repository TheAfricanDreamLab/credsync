//! The state machine itself — at CS-6, its shape and nothing more.
//!
//! State transitions arrive at CS-7 (#8) onward. There is deliberately no `handle` method yet: a
//! `handle` that accepted an [`Event`](crate::Event) and did nothing would compile, look
//! finished, and quietly swallow every event fed to it. An absent method is a compile error at
//! the call site, which is the failure anyone would rather have.

use crate::effect::Effect;
use crate::traits::{Clock, Entropy, Storage, Transport};
use core::fmt;
use std::collections::VecDeque;

/// The credSync client state machine.
///
/// # Single-threaded by construction
///
/// No method, field, or bound here mentions `Send` or `Sync`, and none may. One engine is driven
/// by one thread.
///
/// This is not caution about concurrency — it is a constraint the consumers actually impose.
/// Hermes, React Native's JavaScript engine, is single-threaded; a `Send` bound here would force
/// every binding to wrap its SQLite handle in a mutex to satisfy a requirement the design never
/// had. `tests/single_threaded.rs` constructs this engine from four deliberately `!Send` parts,
/// so adding such a bound stops compiling rather than merely becoming regrettable.
///
/// # Why it owns all four traits
///
/// Design v2.1 §4.1 describes storage results arriving as events, which would put `Storage`
/// outside the engine. It is held here instead (D-037): `Storage::transact` answers immediately,
/// so routing its result back through the event queue would mean parking a half-finished apply
/// across a round trip — the exact state that must never be interruptible. `Transport` differs
/// and is answered by [`Event::TransportResponse`](crate::Event::TransportResponse), because a
/// request handed to the network is answered later or never.
pub struct Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    clock: C,
    entropy: E,
    storage: S,
    transport: T,
    /// Queued for the caller to drain. Ordered: effect order is part of the engine's observable
    /// behaviour, and the simulator asserts two runs of one seed produce the same stream.
    effects: VecDeque<Effect>,
}

impl<C, E, S, T> Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    /// Builds an engine over the four supplied implementations.
    pub const fn new(clock: C, entropy: E, storage: S, transport: T) -> Self {
        Self {
            clock,
            entropy,
            storage,
            transport,
            effects: VecDeque::new(),
        }
    }

    /// Takes the next queued effect, oldest first.
    ///
    /// Returns `None` when the queue is empty, which is the normal resting state — an engine with
    /// nothing to say is an engine with nothing to do.
    pub fn next_effect(&mut self) -> Option<Effect> {
        self.effects.pop_front()
    }

    /// How many effects are waiting.
    #[must_use]
    pub fn pending_effects(&self) -> usize {
        self.effects.len()
    }
}

/// The handles the transitions will reach for.
///
/// `dead_code` is allowed here, and only here, because CS-6 is explicitly a skeleton: the issue
/// puts every state transition in CS-7 (#8) and later. The accessors exist now so the four
/// implementations are *stored* now, which is what fixes the struct's bounds — and fixing them
/// now is the point of the slice, since `tests/single_threaded.rs` asserts against exactly these
/// bounds.
///
/// If this attribute is still here once transitions exist, it is stale and should go.
#[allow(dead_code)]
impl<C, E, S, T> Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    /// Queues an effect for the caller.
    ///
    /// Crate-private: effects are produced by transitions, never by a caller reaching in.
    pub(crate) fn emit(&mut self, effect: Effect) {
        self.effects.push_back(effect);
    }

    /// The injected clock.
    pub(crate) const fn clock(&self) -> &C {
        &self.clock
    }

    /// The injected entropy source.
    pub(crate) fn entropy_mut(&mut self) -> &mut E {
        &mut self.entropy
    }

    /// The injected storage.
    pub(crate) fn storage_mut(&mut self) -> &mut S {
        &mut self.storage
    }

    /// The injected transport.
    pub(crate) fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
}

// Written by hand rather than derived: `#[derive(Debug)]` would add `C: Debug` and three more
// bounds, so a caller whose SQLite handle is not `Debug` could not debug-print the engine. The
// four implementations are also the least interesting thing about it — what a reader wants is
// how much work is outstanding.
impl<C, E, S, T> fmt::Debug for Engine<C, E, S, T>
where
    C: Clock,
    E: Entropy,
    S: Storage,
    T: Transport,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("pending_effects", &self.effects.len())
            .finish_non_exhaustive()
    }
}
