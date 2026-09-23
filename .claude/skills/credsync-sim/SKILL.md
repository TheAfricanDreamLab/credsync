---
name: credsync-sim
description: Deterministic simulation testing for credSync - how to add a fault, an invariant, or a scenario; the seed-replay workflow; reading coverage statistics; and the planted-bug drill. Use when working in credsync-sim, adding any invariant, investigating a sim failure, or making a behavioural claim that needs proving under network chaos.
---

# Deterministic simulation

The simulator drives N simulated devices and one simulated server — the **real** core and **real**
server logic, with fake `Clock`/`Entropy`/`Storage`/`Transport` — through seeded runs. Every packet
delay, drop, duplication, reorder, crash, and clock skew flows from one RNG seed.

**A bug report is one integer.** If a run fails, the seed replays it exactly.

This is the project's central trust artifact. In the tradition of FoundationDB and TigerBeetle:
simulated weeks of flaky-network device life run in seconds of CPU.

## Determinism is the whole asset — protect it

Nondeterminism does not announce itself. A sim that has quietly lost determinism still passes,
still looks busy, and no longer finds anything.

Sources that leak in through the side door:

- Any real clock or RNG reached through a dependency, not just directly
- `HashMap` iteration order (randomised per process)
- Thread scheduling — the core is single-threaded for exactly this reason
- Floating-point accumulation order
- Any address- or pointer-derived value that reaches output

**Guard it with a test**: run the same seed twice, assert byte-identical traces. If that test ever
fails, stop and fix it before anything else — every other sim result has become meaningless.

## Adding a fault

The menu lives in `credsync-sim/src/fault.rs`; the world that applies it is `world.rs`.

**Every branch of a fault decision must draw the same number of times from the generator**, used
or not. A decision whose cost depended on its outcome would shift every later decision, so two
runs differing in one early coin flip would diverge wildly instead of comparably — determinism
preserved, reproducibility made useless for narrowing anything down. `decide_response` draws all
four chances up front and then chooses; keep that shape.

1. Add the variant to the fault menu, and to `Fault::ALL` — the coverage report iterates that
   list, so a variant missing from it is invisible.
2. Drive it from the seeded RNG — never from a real source of chance.
3. Give it a tunable probability; register it in the distribution table.
4. Run a 1,000-seed batch and confirm the fault **actually occurs** (see coverage below).

The full menu (Design §7.1): drop every Nth request; duplicate and reorder responses; sever
mid-batch; 90-second flaps; process kill between storage transaction and ack; storage transactions
that fail after partial visibility; device clocks skewed ±3 days and drifting; server restart with
cold cache; malformed and truncated wire bytes.

## Adding an invariant

An invariant is a claim that must hold at **every step**, not just at quiescence — a bug that
self-corrects before the run ends is still a bug.

The standing set:

| Invariant | Claim |
|---|---|
| Convergence | After quiet, every device's scope digest equals the server's |
| Durability | An acknowledged command's effect exists in every future state |
| Idempotency | Any command applied N times equals once |
| Cursor monotonicity | A cursor never moves backwards |
| No-loss | An outbox entry leaves only into applied or rejected-with-reason |
| Policy conformance | Per entity class, generated from the registry declaration |

**Never add an invariant you have not seen fail.** Break the code deliberately, watch the
invariant catch it, then revert. An invariant that has never fired is untested — it may be
asserting something trivially true, or nothing at all.

## Running it

```sh
cargo run --release -p credsync-sim -- --seeds 1000    # the batch
cargo run --release -p credsync-sim -- --seed 0x4f21a9c3 --trace
```

**Use `--release`.** A debug build is roughly forty times slower, which turns a 22-minute batch
into most of a day and quietly turns the batch into something nobody runs.

Measured at CS-11 on a 2019 x86 laptop: one seed is ~1 second and covers a fortnight of device
life across 2–4 devices. A 1,000-seed batch takes ~22 minutes and simulates about 38 years.

## Seed replay

CI prints the seed on failure. To reproduce:

```sh
cargo run --release -p credsync-sim -- --seed 0x4f21a9c3 --trace
```

Same seed, same trace, always. If a printed seed does not reproduce, **that is a more serious bug
than whatever you were chasing** — determinism has been lost. Fix it first.

Bug issues carry the seed in the title: `sim: divergence at seed 0x4f21a9c3`.

## Coverage hunting

The honest caveat from the field (Design §11): **a DST rig can look busy while exploring almost
nothing.** A million seeds that all follow the same path prove one path.

So coverage is scheduled recurring work (CS-30), not setup:

- Generate the explored-state report; read which faults actually fired and which interleavings
  occurred
- Look for faults with near-zero occurrence — a fault that never fires is a fault you do not have
- Tune the distributions, then re-run and compare
- Record the reasoning, so the next tuning pass starts from evidence rather than instinct

## The planted-bug drill

The only way to know whether the harness works is to give it bugs to find.

1. On a throwaway branch, introduce three deliberate bugs — one ordering, one dedupe, one conflict
2. Run the batch. The harness must catch **all three**
3. Record time-to-detection: how many seeds before each surfaced
4. Confirm each replays from its printed seed
5. Discard the branch. **No planted bug reaches `main`**

Run at CS-13, and repeated with a **new bug class** at CS-30. Reusing the same three bugs only
proves the harness still catches bugs it has already been tuned to catch.

## Never do this

- **Never weaken a fault distribution to make CI pass.** The correct response to a red batch is a
  fix, or a bug issue with its seed. This is the one forbidden move in the repo.
- **Never skip a seed batch** to unblock a merge.
- **Never mark a flaky sim test as ignored.** In a deterministic simulator there is no such thing
  as flaky — apparent flakiness *is* the bug, and it is the most valuable kind you will find.
