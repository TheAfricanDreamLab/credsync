//! Benchmark closing `DECISIONS.md` O-001: xxh3 versus BLAKE3 for checksums and digests.
//!
//! Run with:
//!
//! ```sh
//! cargo run --release --example hash-benchmark -p credsync-protocol
//! ```
//!
//! Release mode matters — both implementations rely on SIMD that a debug build discards, and a
//! debug measurement would compare two things neither of which ships.
//!
//! # On "target-class hardware"
//!
//! The device that matters is a low-end Android phone, not a development machine. Absolute
//! numbers here are therefore not the answer; the **ratio** is, because both algorithms scale
//! with the same memory bandwidth and SIMD width, and the ratio is what decides the tradeoff.
//! A phone will be slower at both, in roughly the same proportion.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]

use std::time::Instant;
use twox_hash::XxHash3_128;

/// Sizes chosen from the limits in `docs/spec.md` §2.1 rather than round numbers.
const CASES: &[(&str, usize)] = &[
    ("command payload, small", 256),
    ("snapshot, typical", 4 * 1024),
    ("payload limit", 64 * 1024),
    ("batch budget", 100 * 1024),
    ("snapshot limit", 256 * 1024),
];

fn main() {
    println!("credSync hash benchmark - closes DECISIONS.md O-001");
    println!(
        "build: {}\n",
        if cfg!(debug_assertions) {
            "DEBUG (results are meaningless, re-run with --release)"
        } else {
            "release"
        }
    );

    println!(
        "{:<24} {:>10} {:>14} {:>14} {:>8}",
        "case", "bytes", "xxh3 MB/s", "BLAKE3 MB/s", "ratio"
    );
    println!("{}", "-".repeat(74));

    let mut ratios = Vec::new();

    for (label, size) in CASES {
        // Deterministic, incompressible-ish input. Neither algorithm is content-sensitive, but
        // using a fixed pattern keeps the run reproducible.
        let data: Vec<u8> = (0..*size)
            .map(|i| (i.wrapping_mul(31) % 251) as u8)
            .collect();

        let iters = (32 * 1024 * 1024 / size).max(64);

        let xxh3 = throughput(iters, *size, || {
            std::hint::black_box(XxHash3_128::oneshot(std::hint::black_box(&data)));
        });
        let blake3 = throughput(iters, *size, || {
            std::hint::black_box(blake3::hash(std::hint::black_box(&data)));
        });

        let ratio = xxh3 / blake3;
        ratios.push(ratio);
        println!("{label:<24} {size:>10} {xxh3:>14.0} {blake3:>14.0} {ratio:>7.1}x");
    }

    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    println!("\nxxh3 is {mean:.1}x faster than BLAKE3 across these sizes.");
    println!("\nVerdict is recorded in docs/spec.md section 5 and DECISIONS.md O-001.");
}

/// Median-of-three timing, reported in MB/s.
///
/// Median rather than mean: a scheduler hiccup skews a mean and would silently misreport the
/// ratio this decision rests on.
fn throughput(iters: usize, size: usize, mut f: impl FnMut()) -> f64 {
    // Warm up so the first measured run is not paying for cold caches and branch predictors.
    for _ in 0..iters / 4 {
        f();
    }

    let mut runs = [0f64; 3];
    for run in &mut runs {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        let elapsed = start.elapsed().as_secs_f64();
        *run = (size * iters) as f64 / elapsed / (1024.0 * 1024.0);
    }
    runs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    runs[1]
}
