//! The sans-IO deterministic state machine at the heart of a credSync client.
//!
//! # The sans-IO rule
//!
//! This crate performs **no I/O and never consults the real world**. It is a state machine: feed
//! it events, drain effects. Everything that touches reality arrives through four traits —
//! `Clock`, `Entropy`, `Storage` and `Transport` — supplied by the caller.
//!
//! In production those are real implementations. In [`credsync_sim`] they are seeded fakes. The
//! same core bytes run in both worlds, and that is the entire trick that makes deterministic
//! simulation possible.
//!
//! This is not a stylistic preference. A single hidden clock read destroys deterministic replay
//! silently: the simulator keeps passing while quietly losing the ability to find bugs.
//!
//! # What must never appear in this crate
//!
//! No `tokio` or any executor, no `Instant::now` or `SystemTime::now`, no `thread::sleep`, no
//! `rand`, no HTTP client, no filesystem or database access, and no `unsafe`. CI asserts the
//! dependency graph is free of I/O crates; see `.claude/skills/rust-sans-io/SKILL.md` for the
//! full ban list and the reasoning behind each entry.
//!
//! # Status
//!
//! Scaffolded at CS-1. The four traits and the event/effect surface arrive at CS-6.
//!
//! [`credsync_sim`]: https://github.com/TheAfricanDreamLab/credsync/tree/main/credsync-sim

#![forbid(unsafe_code)]
