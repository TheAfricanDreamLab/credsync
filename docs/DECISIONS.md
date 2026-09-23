# Decision register

Every binding decision on this project, with its source. This file answers **"what was decided,
and where does it come from"**.

It is deliberately **not** instructions. `CLAUDE.md` holds the operating rules a session follows;
this file holds the decisions those rules rest on. No rule appears in both — if you are looking
for what to *do*, read `CLAUDE.md`; if you are looking for *why*, read here.

Every entry cites a section of a committed document. CI verifies those citations resolve, so a
stale entry fails the build rather than quietly misleading a session.

**Documents referenced**

| Short form | File |
|---|---|
| Design | [credsync-design-v2.1.md](credsync-design-v2.1.md) |
| Plan | [platform-plan-v1.1.md](platform-plan-v1.1.md) |
| Playbook | [execution-playbook-v1.0.md](execution-playbook-v1.0.md) |
| Slice Plan | [build-slice-plan-v1.0.md](build-slice-plan-v1.0.md) |
| Spec | [spec.md](spec.md) — written at CS-2 |

---

## Architecture

| # | Decision | Source | Date |
|---|---|---|---|
| D-001 | **Rust for both core and server.** The client core must embed in React Native, native iOS/Android, and WASM; Go has no credible path there. Splitting core (Rust) from server (Go) would forfeit one protocol crate and one simulation harness exercising both in the same seeded run. | Design §3.1 | 24 Aug 2026 |
| D-002 | **The core is sans-IO.** `credsync-core` performs no I/O and never consults the real world. `Clock`, `Entropy`, `Storage`, `Transport` are trait parameters. This is what makes deterministic simulation possible, and is also the beginner-friendliest shape — the core is plain synchronous Rust. | Design §4.1 | 24 Aug 2026 |
| D-003 | **The protocol spec has a four-page ceiling.** If it grows past four pages, scope has crept. The ceiling is a scope alarm, not a formatting preference, and is asserted mechanically in CI. | Design §5 | 24 Aug 2026 |
| D-004 | **Snapshots down, commands up.** State flows down as ordered full-row snapshots from an append-only log; writes flow up as domain commands the host validates. Business data is never merged by guesswork. | Design §1 | 24 Aug 2026 |
| D-005 | **No CRDTs, no peer-to-peer, no partial-field diffs, no realtime transport.** The strongest teams in the field (ElectricSQL, Replicache) walked back automatic merge. A push notification is only ever a hint to run the loop. | Design §2, §5.3 | 24 Aug 2026 |
| D-006 | **Domain logic never lives in `credsyncd`.** Validated commands are forwarded to the host's registered endpoint. This keeps the engine backend-agnostic: any stack that can expose one HTTP endpoint and write two tables can adopt credSync. | Design §4.2 | 24 Aug 2026 |
| D-007 | **Canonical encoding is compact JSON in v1**, with the codec isolated in `credsync-protocol` so a binary encoding (postcard/CBOR) can arrive as protocol v2 without touching the state machine. Debuggability wins while the protocol is young. | Design §5.3 | 24 Aug 2026 |
| D-008 | **Three conflict classes, declared not conventional.** Server-authoritative (pull-only), owner-draft LWW-per-field, and append-only. The entity registry enforces the class; the simulator generates invariants per class, so a policy claim is a tested property. | Design §6 | 24 Aug 2026 |
| D-009 | **Silent loss is a protocol violation, not a tradeoff.** A losing LWW version returns to the device and is stored as a recovered draft. | Design §6 | 24 Aug 2026 |

## Licensing and ownership

| # | Decision | Source | Date |
|---|---|---|---|
| D-010 | **Apache-2.0 across every crate and package.** Ukeme owns the project; Dream Lab OS and MySaurify consume it as ordinary dependencies. Chosen over MIT for its explicit patent grant. | Design §1 | 24 Aug 2026 |
| D-011 | **Every standalone dependency carries a licence file.** Every crate declares `license = "Apache-2.0"` and carries `LICENSE` + `NOTICE` at its root; `cargo-deny` fails the build on any dependency outside the allowlist (Apache-2.0, MIT, BSD-2/3, ISC, Unicode-3.0, Zlib). Copyleft is a blocking defect. | Slice Plan §3 | 25 Aug 2026 |
| D-012 | **Apache-2.0 grants no trademark rights.** Dream Score™, DBPI™, and DSI™ remain African Dream Network marks regardless of code licence. Stated in `NOTICE`. | Slice Plan §3 | 25 Aug 2026 |
| D-013 | **Commits and PRs are authored by Ukeme alone.** No co-author trailers, no tool attribution in PR bodies, no third party in `NOTICE`, `AUTHORS`, or package metadata. Held by convention, not by a CI job — a gate policing commit trailers is process theatre. | Owner instruction | 25 Aug 2026 |

## Repository and process

| # | Decision | Source | Date |
|---|---|---|---|
| D-014 | **Canonical repo is `TheAfricanDreamLab/credsync`.** A dedicated `credsync` GitHub org is unavailable — `github.com/CredSync` is an existing user account (id 232665566), verified 25 Aug 2026. `ukemeikot/credsync` is a fork kept as a personal copy; CI, milestones, and all registry publishing run from the org repo. | Slice Plan §1 | 25 Aug 2026 |
| D-015 | **Registry names are free and reserved at CS-5.** `credsync` and `cred-sync` are unregistered on crates.io and npm; `@credsync` is a free npm scope. Verified 25 Aug 2026. | Slice Plan §1 | 25 Aug 2026 |
| D-016 | **Slice N is GitHub issue N+1.** GitHub issue numbers start at 1, so `CS-0` is issue #1 and `CS-32` is issue #33. Titles and branches carry the **slice ID**; only `Closes #N` uses the GitHub number. This departs from Playbook §2, which specifies `feat/cs-<issue#>` — the Playbook did not anticipate the offset, and a branch named for the plan is more useful than one named for GitHub's counter. Corrected at CS-1, after CS-0 shipped with the slice ID and contradicted its own written rule. **The offset applies only to the seeded CS-0..CS-32 backlog**; issues filed after seeding (from #36) carry no slice ID and are branched on a descriptive slug instead. | Slice Plan §1; corrected CS-1 | 25 Aug 2026 |
| D-017 | **The full 33-issue backlog is seeded up front.** Unlike the Dream Lab backlog, credSync's design is complete and this is protocol work, not UI awaiting designs — so the issues will not rot, and milestone completion becomes a real progress bar. `scripts/seed-backlog.sh` is idempotent: it updates by title rather than duplicating, so a Slice Plan revision reconciles GitHub with the document. | Playbook §9; owner choice | 25 Aug 2026 |
| D-018 | **`docs/*.md` is generated, never hand-edited.** The planning documents are authored in Word; the markdown is produced by `scripts/convert-docs.sh` from `docs/source/*.docx`. The conversion fails the build on any ragged table row or on a table-count mismatch against the source — the two ways a conversion silently degrades. Figures are extracted to `docs/images/` rather than inlined as base64, which would bloat the markdown roughly 24x. | This slice (CS-0) | 25 Aug 2026 |
| D-019 | **CS-0 CI checks the repo; CS-1 CI checks the code.** There are no crates until CS-1, so a Rust job at CS-0 would have nothing to compile. Required status checks are attached at CS-1, because a required check naming a workflow that does not exist blocks every PR. | Slice Plan §5 | 25 Aug 2026 |
| D-020 | **The org is on the GitHub Free plan.** Branch protection is therefore unavailable on the one private repo (`dreamlab-infra`) — confirmed empirically. That repo compensates with a committed pre-push hook, advisory CI, and holding no application logic. All eight public repos, credsync included, have enforced protection. | Slice Plan §3.2 | 25 Aug 2026 |

## Toolchain

| # | Decision | Source | Date |
|---|---|---|---|
| D-021 | **Rust toolchain is pinned to an exact stable version**, currently **1.98.0** (released 2026-08-18), via `rust-toolchain.toml`. MSRV equals the pin and is declared as `rust-version` in every crate. Bumps are deliberate PRs. A floating toolchain could change behaviour mid-project, which is unacceptable for a system whose core promise is that a seed replays identically forever. | Owner choice | 25 Aug 2026 |
| D-022 | **Edition 2024.** | Owner choice | 25 Aug 2026 |
| D-023 | **`#![forbid(unsafe_code)]` in `credsync-core` and `credsync-protocol`.** `unsafe` appears only in generated FFI scaffolding. Loom is unnecessary by design: the core is single-threaded, and concurrency lives at the bindings' edges and in `credsyncd`, which the simulator exercises. | Design §7.2 | 24 Aug 2026 |

| D-024 | **Workspace clippy lints deny `unwrap`, `expect`, `panic`, `todo` and `unimplemented`.** A silent panic in a decoder is a remote crash; a silent unwrap in the core is lost user data. Denied rather than warned so they cannot accumulate. `iter_over_hash_type` is denied too: `HashMap` iteration order is randomised per process, a real nondeterminism source inside a seeded simulator. | This slice (CS-1) | 25 Aug 2026 |
| D-025 | **Release profile sets `lto`, `codegen-units = 1`, `strip` and `panic = "abort"`.** Set at CS-1 rather than CS-25 because the 3 MB-per-ABI size budget is far easier to hold from the start than to claw back once the core is large. | Design §11 | 25 Aug 2026 |

| D-026 | **CodeRabbit reviews every PR, profile `assertive`.** Playbook §5 makes the CI gates the reviewer, but gates only catch what someone thought to encode. `.coderabbit.yaml` carries the non-negotiables as per-path instructions so the review is against this project's rules rather than generic advice. Generated docs are excluded: reviewing them means reviewing Word's output. | This slice (CS-2) | 26 Aug 2026 |
| D-027 | **The four-page ceiling is enforced as ≤190 lines of `docs/spec.md`.** A line count is a crude proxy for pages but it is unambiguous and cannot be gamed by formatting. Verified to actually fail at CS-2 by padding the file — a gate that cannot fail is worse than no gate, because it reads as coverage. | Design §5; This slice (CS-2) | 26 Aug 2026 |

| D-028 | **`snapshot` and `payload` may contain no floating-point numbers, at any depth.** Discovered at CS-3: `serde_json` float parsing is not exact — `2.2283095771495367e-21` re-parses one ULP away and re-encodes shorter — so a document holding a float does not survive a serialize/parse/serialize cycle unchanged. Since scope digests are computed over the canonical encoding, that would let two sides holding the same logical row compute different digests, and the client would re-bootstrap against a divergence that never happened: the detector firing on itself. Hosts use scaled integers or decimal strings, which are exact and portable where JSON floats are not. | This slice (CS-3); Design §5.2 | 26 Aug 2026 |
| D-029 | **Canonical encoding routes through `serde_json::Value` rather than serializing structs directly.** `serde` emits struct fields in declaration order, so a direct encode would make every digest depend on the order fields happen to be written in — someone tidying a struct would silently change every digest in the system. Going through `Value`, whose `Map` is a `BTreeMap`, sorts keys recursively and removes that coupling. The `preserve_order` feature must never be enabled. | This slice (CS-3) | 26 Aug 2026 |
| D-030 | **`CommandId` is parsed by hand rather than via the `uuid` crate.** The crate's v7 generation features depend on `getrandom`, which is on the sans-IO ban list for `credsync-core` — and core depends on `credsync-protocol`, so it would appear in core's dependency graph and fail the CI gate. Hand-parsing also enforces the version-7 nibble, which the crate does not. | This slice (CS-3); D-002 | 26 Aug 2026 |

| D-031 | **O-001 closed: xxh3 for batch checksums and scope digests, BLAKE3 for command payload checksums, both truncated to 128 bits.** Benchmarked at CS-4 across the sizes the spec actually names (256 B to 256 KB): xxh3 runs 4.6x faster on average, 2.2x-6.3x by size. The split follows the threat model rather than a single winner. Checksums and digests ask *is this intact* on a channel TLS already protects, and run per batch and per row on a battery-powered phone. A command checksum asks *is this the body originally sent* - refusing a mutated replay is a promise no non-cryptographic hash can make, and there is one per command rather than one per row, so the cost lands where it is affordable. Design v2.1 §5.1 draws the same line. Re-run `cargo run --release --example hash-benchmark`. | This slice (CS-4); Design §5.1, §12 | 27 Aug 2026 |
| D-032 | **The scope digest sums per-row hashes in wrapping `u128`, rather than XORing them.** Addition is commutative so order cannot matter, and has an exact inverse so a tombstone restores the digest to precisely its prior value. XOR has both properties too, and is the obvious choice, but it is self-inverse: two identical rows cancel, so a bug that duplicated a row would produce the digest of a scope where the row is absent - and the mechanism whose entire purpose is noticing silent divergence would report agreement. Row fields are length-prefixed for the same reason: without that, `("lesson","a1")` and `("lessona","1")` collide. | This slice (CS-4) | 27 Aug 2026 |
| D-033 | **`twox-hash` rather than `xxhash-rust` for xxh3.** `xxhash-rust` is BSL-1.0, outside the allowlist in `deny.toml`; `twox-hash` is MIT. The licence gate chose the crate, which is the gate working as intended (D-011). | This slice (CS-4) | 27 Aug 2026 |

| D-034 | **`digest` and `checksum` are exactly 32 hex characters, not at most 64.** With O-001 closed the width is decided, so any other length is malformed rather than merely unusual. Enforcing a ceiling instead would let a two-character checksum through the decoder while every producer emits 32 — a gap between what the spec declares and what the wire accepts. Found by review at CS-4, in a divergence introduced by CS-4 itself. | This slice (CS-4); D-031 | 27 Aug 2026 |

| D-035 | **Registry publishing is deferred until the engine exists; CS-5 keeps only the fixtures.** The Slice Plan reserved `credsync` and `cred-sync` on crates.io and npm at CS-5 via `0.0.1` placeholders. Owner decision: credSync is consumed locally — path dependencies and `npm link` — inside the Dream Lab OS repos first, and the registries are approached when there is a real release to put on them. A placeholder claimed months ahead of anything to publish is a name held with nothing behind it, and bundling an irreversible outward act into a slice meant CS-5 could not close on its own proof. Name reservation is now issue #43, milestone M7. **The risk accepted:** all five names were re-verified free on 23 Sep 2026, and a name reserved later is a name someone else can take first. | [Slice Plan](build-slice-plan-v1.0.md) §1; owner decision | 23 Sep 2026 |

| D-036 | **Golden fixtures are committed evidence, and no regeneration script ships with them.** A fixture one command away from agreeing with whatever the code now does proves nothing: the failure it exists to cause would be reflexively regenerated away. So a moved fixture is read as a wire change and answered by updating [spec.md](spec.md) and the fixture together. The same reasoning drives the drill — rather than breaking one fixture by hand once, every byte of every fixture is perturbed on every run and must be rejected, so an assertion that has quietly become vacuous fails immediately instead of passing for months. | This slice (CS-5); [Spec](spec.md) §2 | 23 Sep 2026 |

| D-037 | **The engine owns all four traits; `Storage` answers inline, `Transport` answers by event.** Design §4.1 lists *"storage results"* among the events fed to the core, which would put `Storage` on the caller's side alongside `Transport`. Owner decision at CS-6: the engine holds `Clock`, `Entropy`, `Storage` and `Transport` and calls them directly. `Storage::transact` is synchronous and answers immediately, so routing its outcome back through the event queue would mean parking a half-finished apply across a round trip — and `docs/spec.md` §4 requires that rows and cursor commit together, which is precisely the state that must not be interruptible. `Transport` is different in kind: a request handed to a network that routinely does not answer cannot be awaited, so it keeps `Event::TransportResponse`. `Effect` therefore carries only what the traits cannot express — `ScheduleRetry` and `Emit`. This supersedes the reading of Design §4.1 and the worked example in the `rust-sans-io` skill, both corrected in this slice. | [Design](credsync-design-v2.1.md) §4.1; owner decision | 23 Sep 2026 |

| D-038 | **The sans-IO ban list is checked against the source, not only the dependency graph.** CI has greped `cargo tree` since CS-1, which catches `tokio` and `reqwest` and misses the likelier failure entirely: `SystemTime::now()` needs no dependency, changes no manifest, compiles, and leaves every test green while deterministic replay is silently gone. `scripts/check-sans-io.sh` greps the source of `credsync-core` and `credsync-protocol` for the banned constructs, stripping comment-only lines so the prose explaining the ban does not trip it. The two gates are complements. CLAUDE.md §3 opens with *"a rule that cannot be checked is a wish"*; until CS-6 this particular rule was one. | This slice (CS-6); D-002 | 23 Sep 2026 |

## Open — decided at a named slice

These are deliberately unresolved. Each has an owning slice; none may be settled informally.

| # | Question | Decided at | Source |
|---|---|---|---|
| O-002 | Initial-snapshot transport for large scopes: paginated pull from seq 0 vs a signed snapshot file via the media pipeline. Decide against real Dream Lab data volumes. | **CS-19** (#20) | Design §12 |
| O-003 | Whether `uniffi-bindgen-react-native` is mature enough, or the fallback (UniFFI bindings wrapped in a thin hand-written RN module) is needed. The core is untouched either way. | **CS-27** (#28) | Design §11 |
| O-004 | Whether `credsyncd` ships an optional Postgres change-capture helper, so hosts write only domain tables and triggers fill `sync_changes`. Leaning yes, as a separate opt-in crate. | after **CS-32** | Design §12 |
| O-005 | Web adapter timing: OPFS/IndexedDB storage lands only when the Dream Lab PWA needs offline, not before. | post platform-P3 | Design §12 |

---

## Amending this register

A decision is added in the same PR as the change that makes it real — never in a separate
documentation PR. Superseded entries are struck through and given a pointer to the entry that
replaced them, never deleted: the trail of what was believed and when is the point.
