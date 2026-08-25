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
//! Scaffolded at CS-1. Fault scheduler arrives at CS-11, invariants at CS-12.

#![forbid(unsafe_code)]
