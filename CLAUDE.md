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
`CS-32` is #33). Titles and branches both carry the **slice ID**; only `Closes #N` uses the
GitHub number. So CS-1 is issue #2 on a branch named `feat/cs-1-<slug>`.

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
| `scripts/sync-fork.sh` | Fast-forwards the owner's personal fork after a merge. Idempotent |

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
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

**`cargo test --workspace` needs a Postgres from CS-14 onward.** The server's integration tests run
against a real database and **fail loudly when one is absent** rather than skipping — a suite that
goes green on a machine which never ran it is worse than no suite. One command satisfies it:

```sh
eval "$(./scripts/test-postgres.sh)"     # two throwaway clusters, exports both URLs
./scripts/test-postgres.sh --stop        # when you are done
```

**Two clusters, not one.** The migration tests (#63) hold write transactions open for tens of
seconds deliberately, and `changes_after` decides what to withhold from `pg_snapshot_xmin` — the
oldest transaction running anywhere in the **cluster**, because transaction ids are cluster-wide.
Measured at CS-17: a transaction held open in a completely unrelated *database* still made a
committed row read back as `0` of `1`, appearing the instant it ended. So a second database
isolates nothing and a second cluster does. One command still starts both; the liveness problem
underneath is #62.

They touch nothing else: not your own Postgres, not port 5432, not your data directory. CI uses two
`services: postgres` containers instead.

**`RUSTDOCFLAGS`, not `RUSTFLAGS`.** Cargo forwards `RUSTFLAGS` to rustc and *not* to rustdoc, so
`-D warnings` alone leaves every rustdoc lint — broken intra-doc links included — a warning that
exits 0. CI carried a `cargo doc` step from CS-1 that had never once failed a build, and a broken
link lived on `main` from CS-3 to CS-10 with every pull request green (#44).

From CS-6 onward, also:

```sh
./scripts/check-sans-io.sh                                    # greps the core's SOURCE
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p credsync-core -p credsync-protocol
```

**Two things about that Miri line, both learned the hard way at CS-6.**

`-Zmiri-disable-isolation` is required and is *not* a weakened gate. `proptest` calls
`std::env::current_dir()` to locate its failure-persistence file; Miri's sandbox refuses
`getcwd`, and the run aborts with `unsupported operation` before testing anything. The flag
relaxes Miri's *sandbox*, not its undefined-behaviour checking, which is the part that matters.
Without it the gate does not run at all — and a gate that aborts looks a lot like a gate that
passed.

Miri needs a nightly toolchain, which `rust-toolchain.toml` deliberately does not pin. Install it
with **`--profile minimal`**:

```sh
rustup toolchain install nightly --profile minimal --component miri
```

`--profile default` repeatedly produced a toolchain that rustup reported as installed while
`bin/` held no `cargo` and no `miri` — the symptom is `error: the 'cargo' binary … is not
applicable to the 'nightly' toolchain`, or a `dyld` failure naming `librustc_driver`. Both read
like a broken Miri and are actually a truncated download. `rustup toolchain uninstall nightly`
and reinstall with the minimal profile.

Miri is slow. Cap the property tests while iterating — `PROPTEST_CASES=8` — and let CI run the
full count.

CI runs both of these as required status checks from CS-10 onward, plus a nightly job at
sixteen times the property-test case count. `PROPTEST_CASES` reaches every suite in the
workspace through `common::config`, so one variable retunes all of them.

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

**Review.** CodeRabbit reviews every PR against `.coderabbit.yaml`, which encodes the
non-negotiables above as per-path instructions — the sans-IO ban list, canonical-encoding rules,
the spec's one-rule-one-place requirement, and gates that cannot fail. It runs `assertive`: a
review that mostly agrees with you is worth nothing when there is no second engineer to disagree.
Address its findings or say why not; do not merge past an unanswered one.

## 6. Building on Windows

Rust's msvc target needs the **MSVC linker**, and rustup does not install it. Without it every
crate that links fails — binaries and test harnesses both — and so does `cargo install cargo-deny`,
whose build scripts link too. That last one is easy to misread as a separate problem; it is not.

```powershell
winget install --id Microsoft.VisualStudio.2022.BuildTools --override `
  "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

It installs under **`Program Files (x86)`**, not `Program Files`:

```
C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools
```

Looking in the wrong one makes a successful install look like a failed one. `vswhere.exe`
(`C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe`) reports the real path.

Run cargo inside the MSVC environment:

```powershell
$vc = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cmd /c "`"$vc`" >nul && cargo test --workspace"
```

**One red herring worth knowing.** Under git-bash a missing linker reports as:

```
link: extra operand '...rcgu.o'
Try 'link --help' for more information.
```

That is git-bash's GNU `link` (coreutils) shadowing `link.exe`, and it reads like a PATH-ordering
bug. It is not — `link.exe` is simply absent. PowerShell gives the honest error
(`linker 'link.exe' not found`), so **diagnose linker problems from PowerShell, not git-bash.**

Verified on Windows at CS-1: `cargo test --workspace` and
`cargo deny check licenses bans sources advisories` both pass once Build Tools is present.

CI runs on Linux and is unaffected by any of this.

## 7. Pointers

| For | Read |
|---|---|
| The protocol, conflict classes, hardening | [docs/credsync-design-v2.1.md](docs/credsync-design-v2.1.md) §5–§7 |
| Why any decision was made | [docs/DECISIONS.md](docs/DECISIONS.md) |
| The slice list and what each must prove | [docs/build-slice-plan-v1.0.md](docs/build-slice-plan-v1.0.md) §5 |
| The delivery loop, CI gates, issue anatomy | [docs/execution-playbook-v1.0.md](docs/execution-playbook-v1.0.md) |
| The consumer this exists for | [docs/platform-plan-v1.1.md](docs/platform-plan-v1.1.md) §7–§9, §11 |

Skills live in `.claude/skills/`. Issue templates name which ones a slice needs — load them.

## 8. When you learn something

Anything a future session needs — a build quirk, a pattern, a trap — is committed as a skill edit
or a line here, **in the same PR that discovered it**. The repo gets smarter every slice. A lesson
left in a session transcript is a lesson lost.
