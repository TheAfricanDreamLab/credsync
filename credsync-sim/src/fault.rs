//! The fault menu. Design v2.1 §7.1.
//!
//! Everything the world is allowed to do to a device, and how often. Each fault is drawn from the
//! seeded [`Rng`] and nothing else, so a run is a pure function of its seed.
//!
//! # Probabilities are deliberately hostile
//!
//! These are not the numbers a healthy network produces. They are the numbers that make a week of
//! bad-network device life happen in a second of CPU — the whole point of time compression. A
//! distribution tuned to look realistic would spend most of its cycles simulating things working,
//! which is the one case that needs no testing.
//!
//! **Never lower one of these to make a batch pass.** That is the single forbidden move in this
//! repo (CLAUDE.md §3): the correct response to a red batch is a fix, or a bug issue carrying its
//! seed. A weakened distribution produces a green simulator that has stopped looking.

use crate::rng::Rng;

/// Three days in milliseconds — the clock skew Design §7.1 names by name.
pub const SKEW_MAGNITUDE_MS: i64 = 3 * 24 * 60 * 60 * 1000;

/// How long a connectivity flap lasts, in milliseconds. Design §7.1: ninety seconds.
pub const FLAP_DURATION_MS: i64 = 90_000;

/// How likely each fault is, in percent unless noted.
///
/// Every field is public so a coverage-hunting pass (CS-30) can tune them from evidence, and so a
/// test can turn one fault up to certainty to prove it fires at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultRates {
    /// Drop one request in every `drop_every_nth`. Design §7.1 states this as a period rather
    /// than a probability, because a periodic drop finds retry bugs a random one misses: it lands
    /// on the *same* request of a repeated cycle every time.
    pub drop_every_nth: u32,
    /// Deliver a response twice.
    pub duplicate_response: u32,
    /// Hold a response back so a later one overtakes it.
    pub reorder_response: u32,
    /// Cut a response off partway through — the client sees truncated bytes.
    pub sever_mid_batch: u32,
    /// Corrupt a response's bytes so it fails to decode.
    pub malformed_bytes: u32,
    /// Lose connectivity entirely for [`FLAP_DURATION_MS`].
    pub connectivity_flap: u32,
    /// Fail a storage transaction before it commits. Nothing is written.
    pub storage_fail_before_commit: u32,
    /// Commit a storage transaction and then lose the acknowledgement — the process dies between
    /// the write landing and the engine hearing about it.
    pub storage_commit_then_kill: u32,
    /// Restart the server with a cold cache.
    pub server_restart: u32,
    /// Restart a device, reloading its engine from storage.
    pub device_restart: u32,
    /// Commit a storage transaction, report failure, and **leave the device running**.
    ///
    /// Distinct from `storage_commit_then_kill`, which pairs the same lie with a restart — and the
    /// restart is what made it survivable, because a restarted engine reloads everything from
    /// storage and never consults the state the lie invalidated.
    ///
    /// Without the restart the engine keeps a cursor and digest that are behind what storage holds,
    /// re-fetches changes storage already has, and writes the stale digest back over the correct
    /// one. The rows stay right and the digest regresses permanently (#55). Adding the fault is
    /// what makes the defence testable: the paired version could never reach it.
    pub storage_lie_about_commit: u32,

    /// Put the server under load, so it sheds by delivering less and processing fewer commands.
    ///
    /// Without this the simulator only ever saw a server with spare capacity, so the whole
    /// question of whether shedding loses acknowledged work was unreachable — every run would
    /// have been green whatever the shedding code did, because it never ran.
    ///
    /// The shedding itself is deliberately *silent*: smaller batches with `has_more`, and results
    /// for a prefix of the commands. A server that answered "rejected" because it was busy would
    /// be recording a verdict about work nothing examined (see `pressure`).
    pub overload: u32,

    /// Hold a write's transaction open so it takes a `seq` now and commits later.
    ///
    /// `seq` is allocated when a write starts and the row becomes visible when it commits, so a
    /// later `seq` can appear before an earlier one. Without this fault the simulated server
    /// committed every write instantly, no two writes were ever in flight, and the whole class of
    /// bug was unreachable — a server losing changes this way would have run green forever
    /// (D-063, D-064).
    pub slow_commit: u32,

    /// Send a structurally valid batch that breaks a protocol rule.
    ///
    /// Distinct from `malformed_bytes`, which produces something that does not decode at all. A
    /// buggy or hostile server sends batches that decode perfectly and are still wrong — a
    /// repeated `seq`, a `next_cursor` that does not cover what was sent. The client's ordering
    /// checks exist for exactly this, and without the fault they were never exercised: the
    /// simulated server builds batches from its log, where seqs increase by construction.
    ///
    /// Added at CS-13, because the planted-bug drill found that loosening the ordering check
    /// changed nothing the harness could see.
    pub protocol_violation: u32,
    /// Milliseconds of network latency, drawn uniformly from this range.
    pub latency_ms: (u32, u32),
}

impl Default for FaultRates {
    /// The standing distribution.
    ///
    /// Tuned so that in a 1,000-seed batch every fault fires many times over. A fault that never
    /// fires is a fault you do not have (Design §11), which is the failure mode coverage hunting
    /// at CS-30 exists to find.
    fn default() -> Self {
        Self {
            drop_every_nth: 7,
            duplicate_response: 12,
            reorder_response: 12,
            sever_mid_batch: 6,
            malformed_bytes: 5,
            connectivity_flap: 4,
            storage_fail_before_commit: 6,
            storage_commit_then_kill: 4,
            server_restart: 2,
            device_restart: 3,
            protocol_violation: 5,
            slow_commit: 8,
            overload: 6,
            storage_lie_about_commit: 5,
            latency_ms: (20, 2_000),
        }
    }
}

impl FaultRates {
    /// A world with no faults at all, for tests that need a quiet network.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            drop_every_nth: 0,
            duplicate_response: 0,
            reorder_response: 0,
            sever_mid_batch: 0,
            malformed_bytes: 0,
            connectivity_flap: 0,
            storage_fail_before_commit: 0,
            storage_commit_then_kill: 0,
            server_restart: 0,
            device_restart: 0,
            protocol_violation: 0,
            slow_commit: 0,
            overload: 0,
            storage_lie_about_commit: 0,
            latency_ms: (10, 10),
        }
    }
}

/// What the world decided to do to one request or response.
///
/// Recorded in the trace, so a replay shows not just that a run failed but which faults it hit on
/// the way there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// The request never arrived.
    Dropped,
    /// The response arrived twice.
    Duplicated,
    /// The response was held back and overtaken.
    Reordered,
    /// The response was cut off partway.
    Severed,
    /// The response's bytes were corrupted.
    Malformed,
    /// Connectivity was lost for a while.
    Flap,
    /// A storage transaction failed before committing.
    StorageFailed,
    /// A storage transaction committed, and then the process died before the engine was told.
    StorageCommittedThenKilled,
    /// The server restarted with a cold cache.
    ServerRestarted,
    /// A device restarted and reloaded from storage.
    DeviceRestarted,
    /// The server sent a batch that decoded cleanly and broke a protocol rule.
    ProtocolViolation,
    /// A write took its `seq` and stayed uncommitted for a while.
    SlowCommit,
    /// The server went under load and started shedding.
    Overloaded,
    /// A storage transaction committed and reported failure, with the device still running.
    StorageLiedAboutCommit,
}

impl Fault {
    /// A short, stable name for the trace and the coverage report.
    ///
    /// Stable because the determinism check compares traces byte for byte: renaming one of these
    /// changes every trace, which is fine, but it must not change between two runs of one seed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dropped => "dropped",
            Self::Duplicated => "duplicated",
            Self::Reordered => "reordered",
            Self::Severed => "severed",
            Self::Malformed => "malformed",
            Self::Flap => "flap",
            Self::StorageFailed => "storage-failed",
            Self::StorageCommittedThenKilled => "storage-committed-then-killed",
            Self::ServerRestarted => "server-restarted",
            Self::DeviceRestarted => "device-restarted",
            Self::Overloaded => "overloaded",
            Self::StorageLiedAboutCommit => "storage-lied-about-commit",
            Self::ProtocolViolation => "protocol-violation",
            Self::SlowCommit => "slow-commit",
        }
    }

    /// Every fault, for coverage reporting.
    ///
    /// A `const` list rather than a derived iterator so that adding a variant without adding it
    /// here is visible: the coverage report would show one fewer row than the menu.
    pub const ALL: [Self; 14] = [
        Self::Dropped,
        Self::Duplicated,
        Self::Reordered,
        Self::Severed,
        Self::Malformed,
        Self::Flap,
        Self::StorageFailed,
        Self::StorageCommittedThenKilled,
        Self::ServerRestarted,
        Self::DeviceRestarted,
        Self::ProtocolViolation,
        Self::SlowCommit,
        Self::Overloaded,
        Self::StorageLiedAboutCommit,
    ];
}

/// Decides what happens to one response in flight.
///
/// Order matters and is fixed: a dropped response cannot also be duplicated, and the checks run
/// in a stable sequence so the number of RNG draws per call does not vary with the outcome.
/// Variable draw counts are a classic way to lose determinism — the *next* decision then depends
/// on what the previous one happened to decide.
#[must_use]
pub fn decide_response(rng: &mut Rng, rates: &FaultRates, request_index: u64) -> Option<Fault> {
    // Every branch below draws exactly once, whether or not its result is used, so the generator
    // advances by the same amount on every path through this function.
    let duplicate = rng.chance(rates.duplicate_response);
    let reorder = rng.chance(rates.reorder_response);
    let sever = rng.chance(rates.sever_mid_batch);
    let malformed = rng.chance(rates.malformed_bytes);

    let dropped =
        rates.drop_every_nth > 0 && request_index.is_multiple_of(u64::from(rates.drop_every_nth));

    if dropped {
        Some(Fault::Dropped)
    } else if malformed {
        Some(Fault::Malformed)
    } else if sever {
        Some(Fault::Severed)
    } else if duplicate {
        Some(Fault::Duplicated)
    } else if reorder {
        Some(Fault::Reordered)
    } else {
        None
    }
}

/// Decides what happens to one storage transaction.
#[must_use]
pub fn decide_storage(rng: &mut Rng, rates: &FaultRates) -> Option<Fault> {
    // Every draw happens, every time, whatever the outcome. A short-circuit would change how many
    // numbers the generator consumes depending on which branch won, and the *next* decision would
    // then depend on this one -- a classic way to lose determinism.
    let fail = rng.chance(rates.storage_fail_before_commit);
    let kill = rng.chance(rates.storage_commit_then_kill);
    let lie = rng.chance(rates.storage_lie_about_commit);

    if fail {
        Some(Fault::StorageFailed)
    } else if kill {
        Some(Fault::StorageCommittedThenKilled)
    } else if lie {
        Some(Fault::StorageLiedAboutCommit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_world_produces_no_faults() {
        let rates = FaultRates::none();
        let mut rng = Rng::new(1);
        for i in 1..500 {
            assert_eq!(decide_response(&mut rng, &rates, i), None);
            assert_eq!(decide_storage(&mut rng, &rates), None);
        }
    }

    /// Every fault in the menu is reachable. A fault that cannot fire is a fault you do not have.
    #[test]
    fn every_network_and_storage_fault_can_fire() {
        let mut seen = std::collections::BTreeSet::new();
        let rates = FaultRates::default();
        let mut rng = Rng::new(0xdead_beef);
        for i in 1..20_000 {
            if let Some(f) = decide_response(&mut rng, &rates, i) {
                seen.insert(f.name());
            }
            if let Some(f) = decide_storage(&mut rng, &rates) {
                seen.insert(f.name());
            }
        }
        for expected in [
            "dropped",
            "duplicated",
            "reordered",
            "severed",
            "malformed",
            "storage-failed",
            "storage-committed-then-killed",
        ] {
            assert!(seen.contains(expected), "{expected} never fired");
        }
    }

    /// The number of RNG draws must not depend on the outcome.
    ///
    /// If it did, one decision's result would shift every later decision, and two runs that
    /// differed only in an early coin flip would diverge wildly rather than comparably. That is
    /// determinism preserved but reproducibility made useless for narrowing a bug down.
    #[test]
    fn a_decision_costs_the_same_number_of_draws_either_way() {
        let rates = FaultRates::default();

        let mut a = Rng::new(42);
        for i in 1..200 {
            let _ = decide_response(&mut a, &rates, i);
        }

        let mut b = Rng::new(42);
        for _ in 1..200 {
            // Same call count, different request indices — only the `drop_every_nth` arm differs,
            // and it consumes no draws.
            let _ = decide_response(&mut b, &rates, 3);
        }

        assert_eq!(
            a.next_u64(),
            b.next_u64(),
            "the generator advanced by different amounts down different paths"
        );
    }
}
