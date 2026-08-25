# credsync — standing brief

Offline-first sync engine for apps on unreliable networks. Rust core, Apache-2.0.

Read this first, every session. It tells you **what to do**. For **why**, see
[docs/DECISIONS.md](docs/DECISIONS.md) — the decision register with sources.

---

## 1. The one rule

**No code without an issue, no issue without a definition of done, no done without a merged PR
that closes it.**

Work one issue per session. Start with:

> Work issue #N. Follow gh-issue-flow: restate the slice and definition of done, name the skills
> you'll use, create the branch, then implement. Do not expand scope; anything extra becomes a
> new issue. Finish by opening the PR with "Closes #N" and test evidence.

**Scope discovered mid-slice becomes a new issue.** Never expand a slice in flight. If your
restatement of an issue does not match what the issue says, the issue is unclear — fix the issue
first, before writing code.

Slice IDs and issue numbers differ by one: **slice CS-N is GitHub issue #N+1** (`CS-0` is #1,
`CS-32` is #33). Titles carry the slice ID; branches carry the GitHub number.

## 2. Repo map

| Path | What it is |
|---|---|
| `credsync-protocol/` | Wire types, canonical codec, checksums, digests, versioning rules |
| `credsync-core/` | The sans-IO state machine: sync loop, outbox, cursors, migrations, conflicts |
| `credsync-server/` + `credsyncd/` | Pull pagination, dedupe, result store, scope tokens, host forwarding |
| `credsync-sim/` | Deterministic simulation: fault scheduler, invariants, seed replay |
| `credsync-ffi/` | UniFFI surface over the core's event/effect API |
| `bindings/` | npm packages (`@credsync/react-native`, later `@credsync/web`) |
| `docs/` | **Generated** from `docs/source/*.docx` — never hand-edit. Run `scripts/convert-docs.sh` |
| `docs/spec.md` | The protocol. Written at CS-2. Four pages, hard ceiling |
| `scripts/seed-backlog.sh` | Reconciles GitHub issues with the Slice Plan. Idempotent |

Crates are scaffolded at CS-1; before that the repo is docs and process only.

## 3. Non-negotiables

Each of these is mechanically checked. A rule that cannot be checked is a wish.

1. **The core is sans-IO.** No I/O, no clocks, no entropy inside `credsync-core` — only the
   `Clock`, `Entropy`, `Storage`, and `Transport` traits. No `tokio`, no `Instant::now`,
   no `SystemTime::now`, no `thread::sleep`, no `rand` in the core. CI greps for these.
   *This is not style. A single hidden clock read destroys deterministic replay, silently, and
   the simulator stops being able to find bugs without ever failing.*

2. **`#![forbid(unsafe_code)]`** in `credsync-core` and `credsync-protocol`. `unsafe` appears
   only in generated FFI scaffolding.

3. **`spec.md` is law.** Any wire change updates `spec.md`, the fixtures, and both codec sides
   **in the same PR** — never in separate PRs. The four-page ceiling is asserted in CI.

4. **Every behaviour claim becomes a sim invariant or a property test in the same PR that
   introduces the behaviour.** Not the next PR. Not a follow-up issue.

5. **Never weaken a fault distribution or skip a seed batch to make CI pass.** The correct
   response to a red gate is a fix, or a bug issue carrying its seed. Weakening a gate is the
   one forbidden move in this repo.

6. **Client writes are commands, never row writes.** Server-authoritative entities are pull-only,
   enforced by the entity registry rather than by convention.

7. **Commits are authored by Ukeme alone.** No co-author trailers, no tool attribution in PR
   bodies.

## 4. Testing posture

Testing is not a phase here; it is how a claim becomes true. The engine's entire value is that it
never loses an acknowledged write, and nobody will believe that because the README says so.

**Every slice ships its own proof.** The definition of done on each issue names the executable
check. If you cannot state the check, the slice is not understood yet.

Pick the weakest tool that actually proves the claim:

| Claim shape | Prove it with |
|---|---|
| "This function maps X to Y" | Unit test |
| "This holds for all inputs" | `proptest` — never a handful of examples |
| "The wire format round-trips" | Property test **and** a golden fixture |
| "This survives hostile input" | `cargo-fuzz` target, corpus committed |
| "This holds under network chaos" | A `credsync-sim` invariant, checked continuously |
| "This never happens" | A sim invariant plus a **planted-bug drill** proving the harness would catch it |
| "No undefined behaviour" | Miri in CI |

**Specific standards for this repo:**

- **Invariants are checked continuously, not at quiescence.** A bug that self-corrects before the
  run ends is still a bug.
- **A sim failure is reported by its seed.** The seed is the whole reproduction. Bug issues are
  titled `sim: <symptom> at seed 0x…`.
- **Test the failure path, not just the happy path.** Host timeout, host 5xx, malformed bytes,
  truncated batch, wrong key, skewed clock, kill between transaction and ack. The network will
  eventually hand you garbage; prove you survive it.
- **Never assert a property you have not seen fail.** When adding an invariant, break the code
  deliberately once and confirm the invariant catches it. An invariant that has never failed is
  an untested invariant.
- **Adapters prove themselves against the conformance suite**, not against bespoke tests. Any
  port that passes conformance is correct by construction; that is the whole contract.

## 5. Commands that must pass before any PR

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo deny check licenses
```

From CS-6 onward, also:

```sh
cargo miri test -p credsync-core -p credsync-protocol
```

From CS-11 onward, also:

```sh
cargo run -p credsync-sim -- --seeds 1000     # 10,000 nightly
```

Docs are generated, so if you touched `docs/source/`:

```sh
./scripts/convert-docs.sh      # fails on ragged rows or table-count mismatch
```

Paste the command **and its result summary** into the PR under `## Test evidence`. A PR without
test evidence is not ready, however green CI looks.

## 6. Pointers

| For | Read |
|---|---|
| The protocol, conflict classes, hardening | [docs/credsync-design-v2.1.md](docs/credsync-design-v2.1.md) §5–§7 |
| Why any decision was made | [docs/DECISIONS.md](docs/DECISIONS.md) |
| The slice list and what each must prove | [docs/build-slice-plan-v1.0.md](docs/build-slice-plan-v1.0.md) §5 |
| The delivery loop, CI gates, issue anatomy | [docs/execution-playbook-v1.0.md](docs/execution-playbook-v1.0.md) |
| The consumer this exists for | [docs/platform-plan-v1.1.md](docs/platform-plan-v1.1.md) §7–§9, §11 |

Skills live in `.claude/skills/`. Issue templates name which ones a slice needs — load them.

## 7. When you learn something

Anything a future session needs — a build quirk, a pattern, a trap — is committed as a skill edit
or a line here, **in the same PR that discovered it**. The repo gets smarter every slice. A lesson
left in a session transcript is a lesson lost.
