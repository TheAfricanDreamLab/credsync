//! Fuzzes the `Snapshot` decoder: a row document: bounded at 256 KB, no floats (docs/spec.md 2.2).
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::Snapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<Snapshot>(data);
});
