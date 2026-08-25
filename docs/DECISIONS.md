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
| D-016 | **Slice N is GitHub issue N+1.** GitHub issue numbers start at 1, so `CS-0` is issue #1 and `CS-32` is issue #33. Titles and branches carry the **slice ID**; only `Closes #N` uses the GitHub number. This departs from Playbook §2, which specifies `feat/cs-<issue#>` — the Playbook did not anticipate the offset, and a branch named for the plan is more useful than one named for GitHub's counter. Corrected at CS-1, after CS-0 shipped with the slice ID and contradicted its own written rule. | Slice Plan §1; corrected CS-1 | 25 Aug 2026 |
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

## Open — decided at a named slice

These are deliberately unresolved. Each has an owning slice; none may be settled informally.

| # | Question | Decided at | Source |
|---|---|---|---|
| O-001 | Digest algorithm: xxh3 everywhere vs BLAKE3 for digests — speed against tamper-evidence. Resolve with a benchmark on target-class hardware. | **CS-4** (#5) | Design §12 |
| O-002 | Initial-snapshot transport for large scopes: paginated pull from seq 0 vs a signed snapshot file via the media pipeline. Decide against real Dream Lab data volumes. | **CS-19** (#20) | Design §12 |
| O-003 | Whether `uniffi-bindgen-react-native` is mature enough, or the fallback (UniFFI bindings wrapped in a thin hand-written RN module) is needed. The core is untouched either way. | **CS-27** (#28) | Design §11 |
| O-004 | Whether `credsyncd` ships an optional Postgres change-capture helper, so hosts write only domain tables and triggers fill `sync_changes`. Leaning yes, as a separate opt-in crate. | after **CS-32** | Design §12 |
| O-005 | Web adapter timing: OPFS/IndexedDB storage lands only when the Dream Lab PWA needs offline, not before. | post platform-P3 | Design §12 |

---

## Amending this register

A decision is added in the same PR as the change that makes it real — never in a separate
documentation PR. Superseded entries are struck through and given a pointer to the entry that
replaced them, never deleted: the trail of what was believed and when is the point.
