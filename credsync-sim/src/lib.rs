//! Deterministic simulation harness: fault injection, invariant checking and seed replay.
//!
//! Drives simulated devices and a simulated server through seeded runs using the **real** core
//! and **real** server logic against fake `Clock`, `Entropy`, `Storage` and `Transport`.
//!
//! Every packet delay, drop, duplication, reorder, process kill and clock skew flows from one RNG
//! seed, so simulated weeks of flaky-network device life run in seconds and any failure replays
//! exactly. A bug report here is one integer.
//!
//! # Determinism is the asset
//!
//! Nondeterminism does not announce itself: a simulator that has quietly lost determinism still
//! passes, still looks busy, and no longer finds anything. Guard it with a test that runs the
//! same seed twice and asserts byte-identical traces.
//!
//! # Status
//!
//! Scaffolded at CS-1, fault scheduler and seed replay at CS-11. Invariants arrive at CS-12 and
//! the planted-bug drill at CS-13 — until those land, a run proves the harness runs, not that the
//! engine is correct.

#![forbid(unsafe_code)]

pub mod fakes;
pub mod fault;
pub mod invariant;
pub mod pressure;
pub mod rng;
pub mod server;
pub mod trace;
pub mod world;

pub use fault::{Fault, FaultRates};
pub use invariant::{Invariants, Violation};
pub use pressure::{Load, Pressure};
pub use rng::Rng;
pub use server::Server;
pub use trace::Trace;
pub use world::World;

/// How many steps of simulated time one run covers.
///
/// At one minute per step this is a fortnight of device life, which is the scale `docs/spec.md`
/// keeps describing: *"a three-week-offline device simply walks forward"*. It costs milliseconds.
pub const STEPS_PER_RUN: u32 = 20_160;

/// Runs one seed and returns its trace.
#[must_use]
pub fn run_seed(seed: u64, rates: FaultRates, trace: Trace) -> World {
    let mut world = World::new(seed, rates, trace);
    world.run(STEPS_PER_RUN);
    world
}
