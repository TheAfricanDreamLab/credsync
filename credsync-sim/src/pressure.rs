//! Backpressure: what a server under load is allowed to do, and what it is never allowed to do.
//!
//! # Shedding load is a scheduling decision, never a verdict
//!
//! The one rule this module exists to enforce. A loaded server may deliver **less**, and may
//! decline to look at a command **at all** — but it may never answer a command it did not process.
//! Recording `rejected` because the server was busy would tell a student their work was refused
//! when nothing ever examined it, and the dedupe table would then serve that lie back on every
//! retry, permanently. This is the same rule as D-065 (a failure to reach the host is not a
//! verdict) arriving from the other direction: there the server could not get an answer, here it
//! chose not to ask.
//!
//! So both levers are *silence*, not refusal:
//!
//! - **Pull** returns a smaller batch and sets `has_more`. The cursor covers exactly what was sent,
//!   so the client resumes from where it got to.
//! - **Push** processes a prefix of the commands and returns results only for those. The rest stay
//!   in the client's outbox, because [`Engine::apply_results`] resolves only ids the response
//!   actually names.
//!
//! Neither needs a wire change. `limit_bytes` is already "a client hint — the server may return
//! less, never more" (`docs/spec.md` §2.1), and a partial `results[]` is already the client's
//! problem to retry rather than a special case.
//!
//! [`Engine::apply_results`]: credsync_core::Engine::apply_results

use crate::rng::Rng;

/// The default compressed byte budget, as `docs/spec.md` §2 sets it.
///
/// The simulator's snapshots are small, so at the real budget the byte cap would never bind and
/// every batch would be limited by the row ceiling instead — which would mean shipping a budget
/// nothing had ever exercised. The simulator therefore runs a deliberately small one.
pub const SIM_BUDGET_BYTES: usize = 2_048;

/// The smallest budget shedding may impose.
///
/// Not zero, and not one byte. A budget below the size of a single change would make every batch
/// empty and the scope would never move — which is a wedge, not backpressure. `fill_batch` already
/// guarantees one change always goes even when it alone exceeds the budget (`docs/spec.md` §2), so
/// this floor is belt and braces: the simulator asserts progress, and a floor that silently
/// depended on that guarantee would stop testing it.
pub const MIN_BUDGET_BYTES: usize = 256;

/// How hard the server is currently shedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// Serving normally.
    Normal,
    /// Busy: batches shrink and pushes are processed in part.
    Shedding {
        /// How many steps of shedding remain.
        steps_left: u32,
    },
}

/// The server's load state, and the two levers it may pull.
#[derive(Debug, Clone)]
pub struct Pressure {
    load: Load,
    /// Divides the byte budget while shedding. Drawn from the seed, so a run replays exactly.
    severity: u32,
    /// Total commands the server declined to look at. For the trace, and for asserting that
    /// shedding actually happened rather than the scenario quietly never triggering.
    pub deferred_commands: u64,
    /// How many times shedding has been entered.
    pub episodes: u64,
}

impl Default for Pressure {
    fn default() -> Self {
        Self::new()
    }
}

impl Pressure {
    /// A server under no load.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            load: Load::Normal,
            severity: 1,
            deferred_commands: 0,
            episodes: 0,
        }
    }

    /// Whether the server is currently shedding.
    #[must_use]
    pub const fn is_shedding(&self) -> bool {
        matches!(self.load, Load::Shedding { .. })
    }

    /// Puts the server under load for a seeded number of steps.
    ///
    /// Severity and duration both come from the run's generator rather than a clock, so the whole
    /// episode replays from the seed.
    pub fn overload(&mut self, rng: &mut Rng) {
        // 2..=8: enough to bind the budget hard without collapsing it to the floor every time,
        // which would make every episode look identical.
        self.severity = rng.range(2, 8);
        let steps_left = rng.range(3, 30);
        self.load = Load::Shedding { steps_left };
        self.episodes += 1;
    }

    /// Advances one step, easing off when the episode has run its course.
    pub const fn tick(&mut self) {
        if let Load::Shedding { steps_left } = self.load {
            if steps_left <= 1 {
                self.load = Load::Normal;
                self.severity = 1;
            } else {
                self.load = Load::Shedding {
                    steps_left: steps_left - 1,
                };
            }
        }
    }

    /// The byte budget to serve this pull under.
    ///
    /// Shrinks while shedding, never below [`MIN_BUDGET_BYTES`].
    #[must_use]
    pub const fn budget_bytes(&self) -> usize {
        match self.load {
            Load::Normal => SIM_BUDGET_BYTES,
            Load::Shedding { .. } => {
                let shrunk = SIM_BUDGET_BYTES / self.severity as usize;
                if shrunk < MIN_BUDGET_BYTES {
                    MIN_BUDGET_BYTES
                } else {
                    shrunk
                }
            }
        }
    }

    /// How many of `offered` commands the server will look at this push.
    ///
    /// **At least one, always.** A server that accepted a push and processed none of it would make
    /// no progress while still consuming a round trip, and a client retrying forever against a
    /// permanently loaded server is a livelock rather than backpressure. Taking a prefix guarantees
    /// the outbox drains, however slowly.
    ///
    /// The commands not taken are simply not answered — see the module docs on why silence is the
    /// only honest way to shed.
    pub fn accept_count(&mut self, offered: usize) -> usize {
        if offered == 0 {
            return 0;
        }
        let taken = match self.load {
            Load::Normal => offered,
            Load::Shedding { .. } => {
                let share = offered / self.severity as usize;
                share.max(1)
            }
        };
        self.deferred_commands += (offered - taken) as u64;
        taken
    }
}
