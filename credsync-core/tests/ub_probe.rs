//! TEMPORARY — CS-10 DoD 4. Deliberately introduced undefined behaviour, to prove the Miri job
//! catches it. Reverted in the next commit; this file must not survive the slice.
//!
//! Integration tests are separate crates, so `credsync-core`'s `#![forbid(unsafe_code)]` does not
//! reach here — which is exactly why this is the right place to plant it. The library itself
//! stays unable to express UB at all.

#[test]
fn deliberate_ub_for_the_miri_gate() {
    let v: Vec<u8> = vec![1, 2, 3];
    // Reads one element past the end. Native `cargo test` will very likely not notice: the read
    // lands in allocated capacity or adjacent heap and returns a plausible-looking byte. That is
    // the whole point of the demonstration — this is precisely the class of bug that passes a
    // green test suite and corrupts data on a device months later.
    let out_of_bounds = unsafe { *v.as_ptr().add(3) };
    assert!(out_of_bounds == out_of_bounds);
}
