#!/usr/bin/env bash
#
# seed-backlog.sh - create or update the credsync issue backlog on GitHub.
#
# Generated from Build Slice Plan v1.0 section 5 (docs/build-slice-plan-v1.0.md).
# Idempotent: an issue whose title already exists is UPDATED in place, never duplicated,
# so re-running after a Slice Plan revision reconciles GitHub with the document.
#
# Usage:   ./scripts/seed-backlog.sh            # against TheAfricanDreamLab/credsync
#          REPO=owner/name ./scripts/seed-backlog.sh
#          DRY_RUN=1 ./scripts/seed-backlog.sh  # print what would happen
#
set -euo pipefail

REPO="${REPO:-TheAfricanDreamLab/credsync}"
DRY_RUN="${DRY_RUN:-0}"

M0="M0 - Spec & protocol crate"
M1="M1 - Core state machine"
M2="M2 - Simulator"
M3="M3 - Server"
M4="M4 - Migrations & upgrade"
M5="M5 - FFI & React Native"
M6="M6 - Hardening"
M7="M7 - Release"

created=0; updated=0; skipped=0

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 not found" >&2; exit 1; }; }
need gh

# Cache every existing issue title -> number once, rather than one API call per slice.
declare -A EXISTING
while IFS=$'\t' read -r num title; do
  [ -n "${title:-}" ] && EXISTING["$title"]="$num"
done < <(gh issue list --repo "$REPO" --state all --limit 300 \
           --json number,title --jq '.[] | [.number, .title] | @tsv')

# mk <title> <milestone> <area-label> <kind-label>   ... body on stdin
mk() {
  local title="$1" milestone="$2" area="$3" kind="$4" body num
  body="$(cat)"

  if [ "$DRY_RUN" = "1" ]; then
    printf 'DRY  %-58s [%s] %s/%s\n' "$title" "$milestone" "$area" "$kind"
    return 0
  fi

  num="${EXISTING[$title]:-}"
  if [ -n "$num" ]; then
    gh issue edit "$num" --repo "$REPO" \
      --body "$body" --milestone "$milestone" \
      --add-label "$area" --add-label "$kind" >/dev/null
    printf 'updated  #%-4s %s\n' "$num" "$title"
    updated=$((updated + 1))
  else
    local url
    url="$(gh issue create --repo "$REPO" --title "$title" --body "$body" \
             --milestone "$milestone" --label "$area" --label "$kind")"
    printf 'created  %-6s %s\n' "#${url##*/}" "$title"
    created=$((created + 1))
  fi
}

# ---------------------------------------------------------------- bootstrap --

mk "CS-0 - Bootstrap the delivery system" "$M0" "area:repo" "kind:slice" <<'BODY'
## Context
Execution Playbook v1.0 section 9 (issue zero) and Build Slice Plan v1.0 section 5.
The only issue not preceded by another issue: it installs the delivery system that every
later slice depends on. Its output is process, not product code.

## Slice
Convert the four planning documents to markdown under `/docs` with tables intact; add the
decision register; write `CLAUDE.md`; author the five skills; commit issue and PR templates;
pin the Rust toolchain; add the repo-hygiene CI skeleton; commit and run this seed script.

OUT of scope: any Cargo crate, any Rust source file, any Rust CI job. Those are CS-1.

## Definition of done
- [ ] `/docs` holds all four documents with every table preserved, plus `DECISIONS.md`
- [ ] `CLAUDE.md` committed with the Playbook section 4.1 non-negotiables
- [ ] Five `.claude/skills/*/SKILL.md` committed; `rust-ffi-bindings` marked provisional
- [ ] `.github/` issue + PR templates committed
- [ ] `scripts/seed-backlog.sh` committed and run: 33 issues live, labelled and milestoned
- [ ] CI green on `main`; `rust-toolchain.toml` pins an exact stable version

## Skills to use
gh-issue-flow

## Note
Six checkboxes, one over the Playbook section 3 sizing rule. Accepted: section 9 defines this
as the bootstrap exception, and splitting it is circular - the seed script needs the templates
it would be seeding issues about.
BODY

# ----------------------------------------------------------------------- M0 --

mk "CS-1 - Cargo workspace, crate scaffolds and CI skeleton" "$M0" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 9 (crate layout); Build Slice Plan v1.0 section 5, M0.
Installs the code skeleton that CS-0 deliberately left out.

## Slice
Cargo workspace at the repo root with six crates scaffolded per Design section 9:
`credsync-protocol`, `credsync-core`, `credsync-server`, `credsyncd`, `credsync-sim`,
`credsync-ffi`, plus a `bindings/` directory placeholder. Add the Rust CI jobs and attach
them as required status checks.

OUT of scope: any protocol type, any state-machine logic. Crates are empty but compile.

## Definition of done
- [ ] `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` all green
- [ ] Every crate declares `license = "Apache-2.0"` and carries `LICENSE` + `NOTICE`
- [ ] `cargo deny check licenses` green against the allowlist (Apache-2.0, MIT, BSD-2/3, ISC, Unicode-3.0, Zlib)
- [ ] `cargo tree` shows zero I/O crates in the `credsync-core` graph
- [ ] Required status checks attached to `main` branch protection

## Skills to use
rust-sans-io
BODY

mk "CS-2 - Write spec.md v1 (four pages maximum)" "$M0" "area:protocol" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 sections 5 and 6. The four-page ceiling is a scope-creep alarm, not a
formatting preference: Design section 5 states that if the spec grows past it, scope has crept.

## Slice
Write `/docs/spec.md`: scopes, change log, cursors, commands, pull, push, batch checksums,
scope digests, wire discipline, the three conflict classes, protocol and schema versioning.

OUT of scope: Rust types (CS-3), fixtures (CS-5).

## Definition of done
- [ ] `/docs/spec.md` committed and reviewed
- [ ] Every rule in Design sections 5 and 6 appears exactly once - no rule stated twice, none missing
- [ ] CI asserts the four-page ceiling mechanically
- [ ] Every wire field has a declared explicit size limit

## Skills to use
credsync-protocol
BODY

mk "CS-3 - Protocol types and canonical JSON codec" "$M0" "area:protocol" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 5.3; Platform Plan v1.1 sections 7.3 and 7.4.
Canonical encoding is compact JSON in v1, isolated in `credsync-protocol` so a binary codec
can arrive as protocol v2 without touching the state machine.

## Slice
Rust types and canonical codec for: `Scope`, `EntityRegistration`, `Change`, `PullRequest`,
`PullResponse`, `Command`, `PushRequest`, `PushResponse`, `ConflictClass`, plus protocol and
schema version envelopes.

## Definition of done
- [ ] `proptest` encode/decode round-trip green for every wire type
- [ ] Canonical encoding is byte-stable: same value always encodes to identical bytes
- [ ] Every field enforces its `spec.md` size limit; oversize input is rejected, not truncated
- [ ] Decoder rejects malformed and truncated input without panicking (unit tests per type)
- [ ] `spec.md` and the types agree - any divergence found is fixed in this PR

## Skills to use
credsync-protocol
BODY

mk "CS-4 - Batch checksums and order-independent scope digest" "$M0" "area:protocol" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 sections 5.1 and 5.2. The digest is the anti-silent-corruption defence:
it makes divergence survivable and observable when reality outcreates the test space.
Design section 12 leaves the digest algorithm open - this slice closes it with a benchmark.

## Slice
xxh3 batch checksum over the canonical encoding; order-independent rolling scope digest over
`(entity, entity_id, row_version)` for all live rows; payload checksum on commands.

## Definition of done
- [ ] Digest property tests: order-independence across arbitrary permutations, tombstone handling, `row_version` sensitivity
- [ ] Checksum detects single-bit corruption in every wire type (property test)
- [ ] Benchmark xxh3 vs BLAKE3 on target-class hardware; verdict recorded in `spec.md` and `DECISIONS.md`
- [ ] Digest is incremental: applying N changes one at a time equals applying them as a batch

## Skills to use
credsync-protocol
BODY

mk "CS-5 - Golden fixtures and registry name reservation" "$M0" "area:protocol" "kind:slice" <<'BODY'
## Context
Build Slice Plan v1.0 section 1: `credsync` and `cred-sync` are free on both crates.io and npm,
and `@credsync` is a free npm scope (verified 25 Aug 2026). Names are reserved now, before the
project is public enough for someone else to take them.

## Slice
Golden fixture files for every wire type; placeholder 0.0.1 publishes to reserve names.

## Definition of done
- [ ] Fixtures decode on a clean checkout with no network access
- [ ] Fixture round-trip test fails loudly if the wire format moves without a fixture update
- [ ] `credsync` and `cred-sync` published 0.0.1 to crates.io
- [ ] `credsync`, `cred-sync`, `@credsync/protocol` published 0.0.1 to npm
- [ ] Every placeholder carries the Apache-2.0 licence field

## Skills to use
credsync-protocol
BODY

# ----------------------------------------------------------------------- M1 --

mk "CS-6 - Core skeleton: four traits and the event/effect surface" "$M1" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 4.1. The sans-IO rule is the entire trick that makes deterministic
simulation possible: no hidden clock reads, no ambient randomness, nothing inside the state
machine that touches the real world.

## Slice
Define `Clock`, `Entropy`, `Storage`, `Transport` in `credsync-core` - declarations only, no
implementations. Define the `Event` and `Effect` enums and the engine struct.

OUT of scope: any state transition logic. That is CS-7 onward.

## Definition of done
- [ ] Compiles under `#![forbid(unsafe_code)]`
- [ ] `cargo tree` asserts zero I/O crates in the graph - no tokio, no reqwest, no std::time
- [ ] A CI check fails the build if `Instant::now`, `SystemTime::now`, `thread::sleep`, or `rand` appears in `credsync-core`
- [ ] Engine is single-threaded by construction: no `Send`/`Sync` bounds required

## Skills to use
rust-sans-io
BODY

mk "CS-7 - Pull apply path: cursor walk, ordered apply, digest maintenance" "$M1" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 5; Platform Plan v1.1 section 7.4 ordering guarantee.
Changes within a scope are strictly seq-ordered; clients apply in order and persist the cursor
transactionally with the applied rows.

## Slice
Cursor walk, strictly ordered apply, gap rejection, tombstone handling, incremental digest
maintenance on apply.

## Definition of done
- [ ] Unit tests: in-order apply, out-of-order rejection, gap rejection, duplicate seq handling
- [ ] Tombstone apply removes the row and updates the digest correctly
- [ ] Cursor is persisted in the SAME storage transaction as the applied rows (test asserts atomicity)
- [ ] Property test: applying any valid change sequence yields a digest matching a from-scratch recomputation
- [ ] Interrupted apply leaves cursor and rows consistent (no partial batch visible)

## Skills to use
rust-sans-io
BODY

mk "CS-8 - Outbox: enqueue, push batching, results, dead-letter" "$M1" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 1 and Platform Plan v1.1 risk R1: sync bugs that lose student work
silently are trust-fatal. The outbox is the mechanism that makes "never loses an acknowledged
write" true rather than aspirational.

## Slice
Outbox enqueue, push batching by compressed byte budget, per-command result handling
(applied / rejected / superseded), dead-letter states with reasons.

## Definition of done
- [ ] Property test: no acknowledged command is ever lost across arbitrary result sequences
- [ ] Property test: an entry leaves the outbox only into applied or rejected-with-reason - never silently
- [ ] Batching respects the compressed byte budget, not a row count
- [ ] Rejected commands retain their reason for UI surfacing (dead-letter is visible, not dropped)
- [ ] Replayed results are idempotent: the same result applied twice changes nothing

## Skills to use
rust-sans-io
BODY

mk "CS-9 - Conflict application per entity class" "$M1" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 6. The registry declares each entity's class; the simulator's
invariants are generated per class, so a policy claim is a tested property, not documentation.
Silent loss is a protocol violation, not a tradeoff.

## Slice
Apply logic for the three classes: server-authoritative (pull-only, command targeting rejected
by the registry), LWW-per-field via server `row_version`, and append-only.

## Definition of done
- [ ] Registry rejects a command targeting a server-authoritative entity - enforced by type or runtime check, not convention
- [ ] LWW resolves by server `row_version`; `client_ts` is a tiebreaker hint only
- [ ] Test: a device with a clock skewed +3 days does NOT win an LWW conflict on that basis
- [ ] Losing LWW version is stored locally as a recovered draft (test asserts it is retrievable)
- [ ] Append-only entities: a new version never overwrites a prior one
- [ ] One class-conformance test per class, generated from the registry declaration

## Skills to use
rust-sans-io
BODY

mk "CS-10 - Miri and property-test CI gates" "$M1" "area:core" "kind:hardening" <<'BODY'
## Context
Execution Playbook v1.0 section 5 lists Miri as a merge gate, but no slice wired it.
Build Slice Plan v1.0 section 5 adds this. The gate must land the moment there is core code
to check, not after the core is finished.

## Slice
Wire `cargo miri test` into CI for `credsync-core` and `credsync-protocol`; configure proptest
case counts for PR versus nightly; make both required status checks.

## Definition of done
- [ ] `cargo miri test` green on `credsync-core` and `credsync-protocol`
- [ ] proptest runs a fast case count on PR and a large one nightly
- [ ] Both attached as required status checks on `main`
- [ ] A deliberately introduced UB is caught by the Miri job (verified once, then reverted)

## Skills to use
rust-sans-io
BODY

# ----------------------------------------------------------------------- M2 --

mk "CS-11 - Simulator v0: fake traits, seeded scheduler, fault menu" "$M2" "area:sim" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 7.1 - the centrepiece. Every packet delay, drop, duplication,
reorder, crash, and clock skew flows from one RNG seed. A bug report is one integer.

## Slice
`credsync-sim`: fake `Clock`/`Entropy`/`Storage`/`Transport`, seeded scheduler, and the full
Design section 7.1 fault menu. N simulated devices plus one simulated server running the REAL
core.

## Definition of done
- [ ] Fault menu complete: drop every Nth, duplicate, reorder, sever mid-batch, 90s flap, kill between storage txn and ack, storage failure after partial visibility, clock skew +/-3 days with drift, server restart cold cache, malformed and truncated bytes
- [ ] 1,000-seed batch runs deterministically: the same seed produces a byte-identical trace
- [ ] Failing seed is printed on failure and replays exactly from the CLI
- [ ] Simulated weeks of device life run in seconds of CPU (time compression verified)

## Skills to use
credsync-sim
BODY

mk "CS-12 - Invariants checked continuously" "$M2" "area:sim" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 7.1. Invariants are checked continuously, not just at quiescence -
a bug that self-corrects before the run ends is still a bug.

## Slice
Implement and wire the invariant checkers: convergence, durability, idempotency, cursor
monotonicity, no-loss, and per-class policy conformance.

## Definition of done
- [ ] Convergence: after quiet, every device's scope digest equals the server's
- [ ] Durability: an acknowledged command's effect exists in every future state
- [ ] Idempotency: any command applied N times is equivalent to once
- [ ] Cursor monotonicity: a cursor never moves backwards
- [ ] No-loss: an outbox entry never disappears except into applied or rejected-with-reason
- [ ] Policy conformance generated per entity class from the registry (CS-9)

## Skills to use
credsync-sim
BODY

mk "CS-13 - Planted-bug drill" "$M2" "area:sim" "kind:hardening" <<'BODY'
## Context
credSync Design v2.1 sections 7.1 and 11: a DST rig can look busy while exploring very little.
The drill is how we find out whether the harness actually works before trusting it.

## Slice
Introduce three deliberate bugs on a throwaway branch - one ordering, one dedupe, one conflict -
and prove the harness catches each and replays it from its seed.

## Definition of done
- [ ] All three planted bugs caught by the harness
- [ ] Each replays exactly from its printed seed
- [ ] Time-to-detection recorded per bug (how many seeds before it surfaced)
- [ ] Drill procedure documented in the `credsync-sim` skill so it can be repeated
- [ ] Branch discarded; no planted bug reaches `main`

## Skills to use
credsync-sim
BODY

# ----------------------------------------------------------------------- M3 --

mk "CS-14 - Postgres schema and pull pagination" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 4.2; Platform Plan v1.1 section 7.4.
Batches are capped by COMPRESSED byte budget, never row count - a 100-row batch of large
snapshots must not blow a 2G connection's budget.

## Slice
`sync_changes` and `sync_command_results` migrations; cursor pagination in `credsync-server`.

## Definition of done
- [ ] Integration test against a real Postgres (Docker), not a mock
- [ ] Batches capped by compressed byte budget (default 100 KB); `has_more` drives continuation
- [ ] A single change larger than the budget is still delivered, not stuck in a loop (edge-case test)
- [ ] Pull is strictly seq-ordered within a scope
- [ ] Pull cannot cross scopes regardless of requested cursor values (isolation test)

## Skills to use
credsync-protocol
BODY

mk "CS-15 - Command dedupe, result store, checksum rejection" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 sections 5.1 and 8. A replay with a mutated body must be rejected as a
distinct invalid request rather than deduped as a success - otherwise the dedupe table becomes
a way to launder tampered commands.

## Slice
Dedupe keyed by command id; result store; payload-checksum verification on replay.

## Definition of done
- [ ] Replay of an identical command returns the recorded outcome without re-applying (test asserts no second side effect)
- [ ] Replay with a mutated payload is REJECTED, not deduped as success
- [ ] Dedupe hit-rate metric exposed
- [ ] Concurrent duplicate submission of the same command id applies exactly once
- [ ] Result store survives server restart (integration test)

## Skills to use
credsync-protocol
BODY

mk "CS-16 - Host command forwarding" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 4.2: domain logic never lives in credsyncd. This is what keeps the
engine backend-agnostic - any stack that can expose one HTTP endpoint and write two tables can
adopt credSync.

## Slice
Forward validated commands to the host's registered endpoint; record applied / rejected /
superseded against the command id.

## Definition of done
- [ ] Reference in-process host in tests; all three outcome paths covered
- [ ] Host timeout, host 5xx, and host-unreachable each produce a defined, tested outcome
- [ ] No domain logic in `credsyncd` - asserted by review checklist in the PR
- [ ] Host outcome is recorded atomically with the dedupe entry
- [ ] Rejected commands carry the host's reason through to the client

## Skills to use
credsync-protocol
BODY

mk "CS-17 - credsyncd binary, scope-token validation, scope blocklist" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 8. credSync never decides who may sync what - it enforces what the
host decided. Tenant isolation rides the scope, restating at the sync layer the guarantee that
RLS provides in the database.

## Slice
axum binary wiring; short-lived JWT signature and scope-claim validation; server-side scope
blocklist for immediate revocation ahead of token expiry.

## Definition of done
- [ ] A token is refused for any scope not in its claims
- [ ] Expired, unsigned, wrong-key, and algorithm-confusion tokens are all rejected (test per case)
- [ ] Pull can never cross scopes regardless of client input (adversarial test)
- [ ] Blocklisted scope is cut immediately, before token expiry
- [ ] Tenant isolation test: a token for tenant A returns zero rows for tenant B

## Skills to use
credsync-protocol
BODY

mk "CS-18 - Backpressure and server-in-simulator" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 3.1: the single biggest robustness asset is one protocol crate and
one simulation harness exercising client AND server together in the same seeded run. This slice
is where that becomes real.

## Slice
Backpressure under load; run the real server logic inside the simulator alongside the real core.

## Definition of done
- [ ] The real server logic runs inside the simulator with the real core in one seeded run
- [ ] End-to-end sim scenario green across the full fault menu
- [ ] Backpressure sheds load without dropping acknowledged commands
- [ ] Server restart mid-cycle is a survivable scenario in the sim, not a failure

## Skills to use
credsync-sim
BODY

mk "CS-19 - Initial-snapshot bootstrap endpoint" "$M3" "area:server" "kind:slice" <<'BODY'
## Context
Platform Plan v1.1 section 7.4 requires a bootstrap path for new devices; credSync Design v2.1
section 12 leaves its transport open. Build Slice Plan v1.0 section 5 adds this slice because
CS-22's tainted-scope re-bootstrap has nothing to call without it.

## Slice
Per-scope bootstrap from seq 0, paginated and byte-budgeted, then join the change log.

## Definition of done
- [ ] New-device bootstrap scenario green in the simulator
- [ ] Bootstrap is resumable mid-way (kill and resume test)
- [ ] A device bootstrapping while writes land concurrently converges correctly (no lost or doubled rows)
- [ ] Decision recorded in `spec.md` and `DECISIONS.md`: paginated pull in v1, signed snapshot file deferred
- [ ] Bootstrap respects the same compressed byte budget as ordinary pull

## Skills to use
credsync-protocol
BODY

# ----------------------------------------------------------------------- M4 --

mk "CS-20 - Schema-version migrations and outbox forward-migration" "$M4" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 5; Platform Plan v1.1 section 7.6 and risk R2. A device offline for
three weeks may hold queued commands authored under a schema the server no longer accepts.
Those commands are migrated forward - never dropped.

## Slice
Client-side registered up-migrations for local rows; outbox entries record their authoring
schema version and migrate forward before push.

## Definition of done
- [ ] Sim scenario: v-old client against v-new server through the N-1 window
- [ ] Queued outbox commands migrate forward before push - property test asserts none is dropped
- [ ] Migration composition is associative: applying v1->v2->v3 equals v1->v3 (property test)
- [ ] A migration failure quarantines the row and surfaces it, rather than corrupting it
- [ ] Three-week-offline device catch-up scenario green

## Skills to use
rust-sans-io
BODY

mk "CS-21 - Forced-upgrade envelope and N-1 window" "$M4" "area:protocol" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 5; Platform Plan v1.1 section 7.6.
Server speaks N and N-1. Below N-1, the client must be told to upgrade in a way its core
understands - and must queue rather than drop what it already holds.

## Slice
`protocol_version` negotiation; 426 response carrying a forced-upgrade envelope; client handling.

## Definition of done
- [ ] Server accepts N and N-1, rejects N-2 with 426 and a well-formed envelope
- [ ] Client parses the envelope and surfaces an upgrade prompt
- [ ] Client QUEUES its outbox on forced upgrade - property test asserts nothing is dropped
- [ ] Sim scenario: v1.2 client against v1.5 server through the N-1 window
- [ ] `spec.md` versioning rules and the implementation agree

## Skills to use
credsync-protocol
BODY

mk "CS-22 - Divergence self-heal (tainted scope re-bootstrap)" "$M4" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 5.2 - the anti-silent-corruption defence. Silent divergence is the
class of sync bug that testing missed and users never report until trust is gone. Self-healing
plus a signal, instead of quiet rot.

## Slice
On digest mismatch after apply: mark scope tainted, re-bootstrap from a fresh snapshot (CS-19),
replay the outbox, emit telemetry carrying both digests.

## Definition of done
- [ ] Sim scenario deliberately forces divergence and asserts full recovery
- [ ] Outbox is replayed after re-bootstrap with no command lost or double-applied
- [ ] Telemetry event carries BOTH digests (client and server) for diagnosis
- [ ] A tainted scope does not block other scopes from syncing
- [ ] Repeated divergence on the same scope escalates rather than looping forever

## Skills to use
credsync-sim
BODY

# ----------------------------------------------------------------------- M5 --

mk "CS-23 - credsync-ffi: UniFFI surface over event/effect" "$M5" "area:ffi" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 sections 4.1 and 9. The FFI surface is small and message-shaped - feed
event in, drain effects out - which keeps every binding thin and keeps Hermes' single-threaded
model happy.

## Slice
UniFFI annotations over the core's event/effect surface. Generate and compile Kotlin and Swift
bindings.

## Definition of done
- [ ] Kotlin and Swift bindings generate and compile
- [ ] FFI surface is message-shaped only - no object graph crosses the boundary
- [ ] `unsafe` confined to generated scaffolding; core and protocol still `forbid(unsafe_code)`
- [ ] Round-trip test through the FFI boundary in both languages
- [ ] Rewrite the `rust-ffi-bindings` skill from real experience (it was drafted provisionally at CS-0)

## Skills to use
rust-ffi-bindings
BODY

mk "CS-24 - rusqlite storage adapter" "$M5" "area:ffi" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 4.3. SQLite is compiled into the core's cdylib - one artifact, no
host SQLite version roulette.

## Slice
`Storage` implementation over rusqlite; local schema of mirrored entity tables plus
`credsync_outbox`, `credsync_cursors`, `credsync_meta`.

## Definition of done
- [ ] Adapter passes the FULL conformance suite (the same seeded scenarios as the fake storage)
- [ ] Transactions are genuinely atomic: a kill mid-transaction leaves no partial state (test)
- [ ] SQLite is statically linked; no reliance on a host-provided libsqlite
- [ ] Schema migration path exists for the local store itself
- [ ] Storage errors surface as typed effects, never panics

## Skills to use
rust-ffi-bindings
BODY

mk "CS-25 - Binding build pipeline and size budget" "$M5" "area:ffi" "kind:hardening" <<'BODY'
## Context
credSync Design v2.1 section 11: the xcframework + AAR + Hermes toolchain is genuinely fiddly,
and binary size matters on low-end Android. CI builds device artifacts every commit so breakage
is caught at the commit that caused it.

## Slice
Scripted xcframework and AAR builds; CI artifact builds; enforced size budget.

## Definition of done
- [ ] CI builds aarch64 iOS and Android artifacts on every PR touching `core/` or `ffi/`
- [ ] `.so` size budget of 3 MB per ABI enforced per commit; a regression fails the build
- [ ] arm64 and armv7 only; LTO and strip enabled
- [ ] Size tracked over time so growth is visible before it breaches
- [ ] Build scripts documented and runnable locally, not CI-only

## Skills to use
rust-ffi-bindings
BODY

mk "CS-26 - SQLCipher at-rest encryption" "$M5" "area:ffi" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 8 and Platform Plan v1.1 section 11: an app-level PIN/biometric gate
over the local store, driven by the shared-phone reality of the target users.
Build Slice Plan v1.0 section 5 adds this slice - the Playbook backlog had none.

## Slice
Feature-flagged SQLCipher storage; the host application supplies the key from its PIN gate.

## Definition of done
- [ ] Conformance suite passes with the flag both on and off
- [ ] The database file is unreadable without the key (test asserts it, does not assume it)
- [ ] Key is never persisted by credSync - host supplies it per session
- [ ] Wrong key fails cleanly with a typed error, not a panic or corruption
- [ ] `.so` size budget from CS-25 still met with SQLCipher enabled

## Skills to use
rust-ffi-bindings
BODY

mk "CS-27 - @credsync/react-native package" "$M5" "area:ffi" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 sections 9 and 11. `uniffi-bindgen-react-native` self-describes as
early-stage; the fallback is mature UniFFI bindings wrapped in a thin hand-written RN native
module. The core is untouched either way.

## Slice
Generated Turbo Module plus TypeScript API sugar: subscribe, status, dead-letter surface.

## Definition of done
- [ ] Package builds and publishes with the Apache-2.0 licence field
- [ ] TypeScript API covered by tests against a mock core
- [ ] Dead-letter items are surfaced with their reasons (the honesty UI depends on this)
- [ ] Fallback path documented in `DECISIONS.md` if the RN generator blocks
- [ ] Works under Hermes single-threaded execution

## Skills to use
rust-ffi-bindings
BODY

mk "CS-28 - Expo example app and airplane-mode device demo" "$M5" "area:ffi" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 10, M5 definition of done; Platform Plan v1.1 section 7.9.
This is the slice that proves the entire engine on the hardware it was built for.

## Slice
Reference Expo app syncing the three Dream Lab reference entities: lessons, submissions,
reflections.

## Definition of done
- [ ] Airplane-mode demo on a REAL low-end Android device: write offline, kill the app, reconnect, converge
- [ ] Client and server scope digests match after convergence (verified, not assumed)
- [ ] Demo recorded as evidence
- [ ] Submission survives an app kill mid-write
- [ ] Runs within the Android 9+ / 1.5 GB free storage device floor

## Skills to use
rust-ffi-bindings, expo-offline
BODY

# ----------------------------------------------------------------------- M6 --

mk "CS-29 - cargo-fuzz targets on all wire decoders" "$M6" "area:sim" "kind:hardening" <<'BODY'
## Context
credSync Design v2.1 section 7.2: the network will eventually hand the client garbage.
Every wire-facing decoder is fuzzed because the server treats the client as untrusted always -
and the client must treat the network the same way.

## Slice
`cargo-fuzz` targets for pull batches, command envelopes, and snapshots.

## Definition of done
- [ ] A fuzz target exists for every wire-facing decoder - none omitted
- [ ] 60-second smoke run per decoder on PR; long runs nightly
- [ ] Overnight run clean
- [ ] Any crash becomes an issue carrying its reproducing input, and the input joins the corpus
- [ ] Corpus committed so coverage accumulates across runs

## Skills to use
credsync-sim
BODY

mk "CS-30 - Coverage hunting and fault-distribution tuning" "$M6" "area:sim" "kind:hardening" <<'BODY'
## Context
credSync Design v2.1 sections 7.1 and 11 - the honest caveat from the field: a DST rig can look
busy while exploring little. Coverage hunting is scheduled recurring work, not a one-time setup.

## Slice
Explored-state statistics; tune fault distributions against them; repeat the planted-bug drill
with a new bug class.

## Definition of done
- [ ] Explored-state report generated and committed
- [ ] Fault distributions adjusted against the report, with the reasoning recorded
- [ ] Planted-bug drill repeated with a bug class NOT used in CS-13
- [ ] Measured improvement in state coverage after tuning
- [ ] Report regeneration is a documented, repeatable command

## Skills to use
credsync-sim
BODY

mk "CS-31 - Telemetry surface, docs and quickstart" "$M6" "area:core" "kind:hardening" <<'BODY'
## Context
credSync Design v2.1 section 7.3: per-scope lag, command reject rates by reason, and dedupe hit
rates are the three numbers that predict user-visible sync pain before users report it.

## Slice
Client telemetry set; `credsyncd` metrics; README, docs, and a quickstart.

## Definition of done
- [ ] Client emits: cycle outcomes, retry depth, dead-letter counts, divergence events with digest pairs, migration events
- [ ] Telemetry is privacy-clean - no user data, verified by test
- [ ] `credsyncd` exposes per-scope lag, reject rate by reason, dedupe hit rate
- [ ] Quickstart executed cold by Emanamfon on a clean machine
- [ ] Every friction point found becomes an issue

## Skills to use
credsync-sim
BODY

# ----------------------------------------------------------------------- M7 --

mk "CS-32 - v0.1.0 release and Dream Lab integration handshake" "$M7" "area:core" "kind:slice" <<'BODY'
## Context
credSync Design v2.1 section 10, M7; Build Slice Plan v1.0 section 5.
The reference consumer proves the engine is genuinely backend-agnostic rather than
accidentally shaped around its own tests.

## Slice
Tag v0.1.0; publish all crates and npm packages; wire the reference-server pattern into
`dreamlab-api`'s Learning module for lessons, submissions, and reflections.

## Definition of done
- [ ] All crates and npm packages published at 0.1.0 with correct licence metadata
- [ ] Reference server pattern wired into `dreamlab-api` for the three reference entities
- [ ] Dream Lab student flow passes the conformance scenarios end to end
- [ ] Lesson pull, offline submission, and reflection LWW all verified against a real backend
- [ ] CHANGELOG and migration notes published; tag pushed

## Skills to use
credsync-protocol, credsync-sim
BODY

# ------------------------------------------------------------------ summary --

echo
if [ "$DRY_RUN" = "1" ]; then
  echo "dry run complete - no issues created or modified"
else
  echo "done: ${created} created, ${updated} updated, ${skipped} skipped  (repo: ${REPO})"
fi
