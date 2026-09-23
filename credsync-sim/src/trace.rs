//! What a run did, recorded so two runs can be compared byte for byte.
//!
//! The trace is the determinism check's evidence. Two runs of one seed must produce identical
//! bytes here — not merely the same verdict, which would pass even if the runs took completely
//! different paths to it.
//!
//! # Every line is stable by construction
//!
//! Nothing here formats a pointer, a hash-map iteration, a real timestamp, or anything else whose
//! value could vary between processes. Simulated time is written explicitly, because that is the
//! only clock a run has.

use crate::fault::Fault;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The record of one simulation run.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    lines: Vec<String>,
    faults: BTreeMap<&'static str, u64>,
    /// Off by default. A 1,000-seed batch that recorded every line would spend its time building
    /// strings rather than finding bugs, so lines are kept only when someone asked to see them.
    recording: bool,
}

impl Trace {
    /// A trace that counts faults but keeps no lines.
    #[must_use]
    pub fn counting() -> Self {
        Self::default()
    }

    /// A trace that keeps every line, for `--trace` and for the determinism check.
    #[must_use]
    pub fn recording() -> Self {
        Self {
            recording: true,
            ..Self::default()
        }
    }

    /// Records one event at a simulated instant.
    pub fn event(&mut self, at_ms: i64, device: usize, what: &str) {
        if self.recording {
            let mut line = String::with_capacity(what.len() + 24);
            // Fixed-width time so lines sort and diff cleanly, and so a longer run does not
            // change the shape of earlier lines.
            let _ = write!(line, "{at_ms:012} d{device:02} {what}");
            self.lines.push(line);
        }
    }

    /// Records a fault, both as a line and in the coverage counts.
    pub fn fault(&mut self, at_ms: i64, device: usize, fault: Fault) {
        *self.faults.entry(fault.name()).or_insert(0) += 1;
        self.event(at_ms, device, fault.name());
    }

    /// How many times each fault fired.
    ///
    /// `BTreeMap`, so the report is in a stable order rather than a hash-map's per-process one.
    #[must_use]
    pub const fn faults(&self) -> &BTreeMap<&'static str, u64> {
        &self.faults
    }

    /// How many lines were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether anything was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The whole trace as bytes, for comparing two runs.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.lines.join("\n").into_bytes()
    }

    /// The trace as text, for `--trace`.
    #[must_use]
    pub fn render(&self) -> String {
        self.lines.join("\n")
    }

    /// A one-line summary of which faults fired, for a batch report.
    #[must_use]
    pub fn coverage(&self) -> String {
        let mut out = String::new();
        for fault in Fault::ALL {
            let n = self.faults.get(fault.name()).copied().unwrap_or(0);
            let _ = write!(out, "{}={n} ", fault.name());
        }
        out.trim_end().to_owned()
    }
}
