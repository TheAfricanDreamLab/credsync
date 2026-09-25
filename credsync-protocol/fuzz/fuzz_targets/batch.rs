//! Fuzzes the `Batch` decoder: one pull batch: the client's main diet, and the one carrying snapshots.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::Batch;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<Batch>(data);
});
