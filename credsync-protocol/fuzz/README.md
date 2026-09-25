# Fuzzing the wire decoders

Every type that bytes off the network can become has a fuzz target here. `docs/spec.md` §2.1 says
oversize input is *rejected, not truncated*, and Design v2.1 §7.2 puts it plainly: the network will
eventually hand the client garbage.

```sh
cargo install cargo-fuzz --locked

./scripts/fuzz.sh              # 20s per target, what CI runs on a pull request
./scripts/fuzz.sh 900          # 15 minutes per target, what the nightly job runs
./scripts/fuzz.sh 60 batch     # one target, for chasing something down
```

`cargo-fuzz` needs nightly for `-Zsanitizer`. `rust-toolchain.toml` deliberately does not pin one
(D-021 pins the toolchain the code is *built* with; a checker is not a build target), so every
invocation says `+nightly` explicitly.

## What each target asserts

Two things, and the second is the one worth having.

**It never panics.** A panic in a decoder is a remote crash. On a phone that is indistinguishable
from the app simply not working, and the input that caused it may be a hostile peer, a broken proxy
cache, or a corrupted flash page.

**Anything it accepts round-trips byte for byte.** Checksums and scope digests are computed over the
canonical encoding (`docs/spec.md` §5). A decoder that quietly normalised its input — dropped an
unknown field, coerced a number, accepted two encodings of one value — would leave two peers
computing different digests for a value they both believe they hold. That is silent divergence
arriving through the front door, and checking only "does not panic" would miss all of it.

## When a target crashes

The artifact **is** the bug report.

`cargo fuzz` must run from the crate being fuzzed, not the repository root — `scripts/fuzz.sh`
changes directory for you, this does not:

```sh
cd credsync-protocol
cargo +nightly fuzz run batch fuzz/artifacts/batch/crash-<hash>
```

1. Open an issue carrying the input. Not a description of it — the bytes, base64 if need be. A
   fuzzer finding is only as useful as its reproduction.
2. Copy the artifact into `fuzz/corpus/<target>/` and commit it. The next run then *starts* from
   the input that broke us rather than hoping to rediscover it, which is what stops a fixed bug
   quietly coming back.
3. Fix it, and keep the corpus entry. It costs a few hundred bytes and buys a permanent regression
   check.

## The corpus is committed on purpose

Coverage accumulates across runs only if the inputs survive them. What is committed is a **seed**
corpus — the golden fixtures from `tests/fixtures/`, one or two per target — not the thousands of
units a single session generates. Seeds are chosen for meaning; volume is the nightly job's
business. That job caches its corpus between runs, so coverage compounds night over night without
any of it landing in the repository, and uploads the grown corpus as an artifact for a person to
look at. A corpus that grows unattended becomes a slow checkout for everybody — one twenty-second
session across all eighteen targets produced 76 MB.

Enum targets get **one seed per variant**, not one file listing them all. The golden fixtures store
enums as arrays (`["upsert","delete"]`), which cannot decode as a single `Op` — so a seed copied
straight from the fixture only ever reached the rejection branch. Measured on `op`: 76 coverage
points from the array, 83 from per-variant seeds.

## Adding a wire type

`scripts/check-fuzz-coverage.sh` derives the list of decoders from `wire.rs` rather than reading a
list somebody maintains, so a new type fails CI until it has a target. That is deliberate: a
hand-kept list is correct the day it is written and stops being correct the first time somebody
forgets — which is exactly when the new decoder is the least exercised code in the crate.

It earned this on the day it was written, catching four decoders the hand-written list had missed:
`Op`, `Status`, `ConflictClass` and `ScopeCursor`. All four are small enums or structs, which is
precisely where a decoder accepts a value it should not.
