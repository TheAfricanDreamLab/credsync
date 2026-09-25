//! Fuzzes the `Op` decoder: the upsert/delete discriminant: a two-variant enum is exactly where a decoder accepts a third value by accident.
//!
//! See `_shared.rs` for what is asserted and why rejection is the expected outcome.

#![no_main]

#[path = "_shared.rs"]
mod shared;

use credsync_protocol::Op;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    shared::decodes_and_round_trips::<Op>(data);
});
