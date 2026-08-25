# BUILD SLICE PLAN v1.0

Derived from: Dream Lab OS Platform Plan v1.1 · credSync Design Document v2.1 · Execution Playbook v1.0
Date: 25 August 2026 · Owner: Ukeme (`ukemeikot`) · Org: `TheAfricanDreamLab`

This is the sequencing layer between the three planning documents and GitHub. It resolves the
documents' open items, reconciles their conflicts, fixes the repository and licence matrix, and
expands the Playbook's backlogs into correctly-sized slices.

Committed to `/docs/build-slice-plan-v1.0.md` in every repo at issue zero.

---

## 1. Resolved open items

| Open item | Source | Resolution (verified 25 Aug 2026) |
|---|---|---|
| GitHub org `credsync` availability | Plan §18, Design §12 | **Not available.** `github.com/CredSync` is an existing user account (id 232665566). Canonical repo is `TheAfricanDreamLab/credsync`; no dedicated org. |
| crates.io `credsync` / `cred-sync` | Design §12 | **Both free.** Reserve at CS-5 via 0.0.1 placeholder publish. |
| npm `credsync`, `cred-sync`, `@credsync/*` | Design §12 | **All free.** Reserve the `@credsync` scope at CS-5. |
| Package visibility & licence | Plan §5 vs. builder instruction | **Every repo public under Apache-2.0**, except `dreamlab-infra` (private). Plan §5's "private" markings are superseded. |
| credSync fork ownership | Builder instruction, Design §1 | Org canonical; `ukemeikot/credsync` is a GitHub fork. CI, milestones, issues, and all registry publishing run from the org repo. |
| Reusable package housing | Plan §5 extraction rule | Backend packages (`tenant-core`, `meet-kit`, `notify-hub`, `media-pipeline`, `ai-eval`) live inside `dreamlab-api` with their own LICENSE, version, and CHANGELOG. They graduate to their own repo on second consumer (MySaurify). `contracts` and `blueprint-tokens` are day-one standalone repos — see §2.1. |

## 2. Reconciled conflicts between the documents

### 2.1 — Standalone clients supersede the Turborepo *(builder decision, 25 Aug)*

Playbook §2 specifies one `dreamlab-os` Turborepo containing `api`, `web`, `mobile`, `site`, and
packages. **This is superseded: frontend, backend, and mobile are standalone repos** with
independent lifecycles, CI, and deployment.

The consequence is load-bearing and must be designed for, not discovered later. A monorepo gave
type-sharing for free; separate repos do not. **`dreamlab-contracts` and `blueprint-tokens` are
therefore promoted from workspace packages to published day-one repos** — they are the only
remaining seam between the four consumers, and every cross-repo type guarantee now flows through
them.

The discipline that replaces the monorepo's compiler:

- `dreamlab-contracts` publishes to npm under `@dreamlab/contracts`. `api`, `web`, `mobile`, and
  `site` each pin an **exact** version — never a range.
- A breaking contract change is a **major bump plus one coordinated slice per consumer repo**,
  opened together and closed in dependency order: contracts → api → web/mobile/site.
- Contracts CI runs a compatibility check against the previous published version and labels the
  release `patch` / `minor` / `major` automatically. A silent breaking change is a blocking defect.
- `api` publishes an OpenAPI/ts-rest artefact on every deploy; a nightly job asserts the deployed
  API matches the contract version its consumers pin. Drift opens an issue automatically.

### 2.2 — Cross-repo milestone tracking

Playbook §1 makes milestone completion "the plan's progress bar, visible to Emanamfon without
asking". With eight repos, per-repo milestones no longer add up to one progress bar.

Resolution: an **org-level GitHub Project** — *Dream Lab OS · P0–P7* — spans every repo, with a
`Phase` field mirroring Plan §15. Each repo still carries its own milestones for local ordering;
the Project is the single view. Playbook §1's "top open issue in the current milestone" rule is
read per repo; the Project resolves cross-repo ordering.

A `TheAfricanDreamLab/.github` repo holds org-default issue templates, PR templates, and the
shared `gh-issue-flow` skill, so eight repos do not mean eight copies of the same ceremony.

### 2.3 — dreamlab milestone numbering

Playbook §8 labels DL-7…DL-15 as P1, but Plan §15 defines P1 as credSync v1 — work that lives
entirely in the `credsync` repo. Playbook §1 states "milestones mirror the plans", so **Plan §15
wins**. Corrected mapping:

| Playbook label | Slices | Corrected milestone (Plan §15) |
|---|---|---|
| P0 | DL-1 … DL-6 | P0 Foundations — unchanged |
| — | — | **The dreamlab repos have no P1 issues.** P1 is credsync M0–M5. |
| P1 (as written) | DL-7 … DL-15 | **P2** LMS core (web) + meetings |
| P2 (as written) | DL-16 … DL-20 | **P3** Mobile app |
| P3 (as written) | DL-21, DL-22, DL-23, DL-25 | **P4** Score + Portfolio + Graduation |
| P3 (as written) | DL-24 | **P5** Residency + Mentorship |

DL-25 (graduation engine) moves to P4 because Plan §15's P4 exit criteria explicitly name
"graduation checklist live".

### 2.4 — CS-1 vs. issue zero

Playbook §7's CS-1 duplicates Playbook §9's issue #0. Split: **#0 installs the delivery system**
(docs, skills, templates, labels, milestones, backlog script, root licence); **CS-1 installs the
code skeleton** (workspace, crates, CI, per-crate licence metadata).

### 2.5 — Design v2.1 supersedes Plan §7

Where the Plan describes a TypeScript core (`@credsync/server`, `@credsync/client`), Design v2.1
§1 explicitly supersedes it: the core is Rust, sans-IO, verified by deterministic simulation.
Plan §7.7's package layout is dead; Design §9's crate layout is authoritative.

---

## 3. Repository & licence matrix

**Every repository is created in the `TheAfricanDreamLab` organisation**
(<https://github.com/orgs/TheAfricanDreamLab>). This is settled, not a per-repo decision: the org
is the home for all nine repos, and `gh repo create` is always org-scoped. The single repository
outside the org is `ukemeikot/credsync` — a fork of the org's canonical credsync, kept as a
personal copy per the owner's requirement.

Every repo carries a licence file; so does every published package inside them.

| Repo | Visibility | Licence | Stack | Branch prefix | First slice |
|---|---|---|---|---|---|
| `TheAfricanDreamLab/.github` | Public | Apache-2.0 | Org templates + shared skills | — | ORG-0 |
| `TheAfricanDreamLab/credsync` | Public | Apache-2.0 | Rust workspace + npm bindings | `cs` | CS-#0 |
| `ukemeikot/credsync` | Public (fork of the above) | Apache-2.0 (inherited) | — | — | — |
| `TheAfricanDreamLab/dreamlab-contracts` | Public | Apache-2.0 | TS · Zod + ts-rest | `ct` | CT-1 |
| `TheAfricanDreamLab/blueprint-tokens` | Public | Apache-2.0 | TS design tokens | `bt` | BT-1 |
| `TheAfricanDreamLab/dreamlab-api` | Public | Apache-2.0 | NestJS (Fastify) + Drizzle | `api` | API-1 |
| `TheAfricanDreamLab/dreamlab-web` | Public | Apache-2.0 | Vite + React PWA | `web` | WEB-1 |
| `TheAfricanDreamLab/dreamlab-mobile` | Public | Apache-2.0 | Expo React Native | `mob` | MOB-1 |
| `TheAfricanDreamLab/dreamlab-site` | Public | Apache-2.0 | Astro | `site` | P6 batch |
| `TheAfricanDreamLab/dreamlab-infra` | **Private** | Apache-2.0 (internal) | Dokploy, Cloudflare, runbooks | `inf` | INF-1 |

### 3.1 — Creation order (executable)

All nine are created up front in one pass, so the org reads as a complete project from day one and
no later slice is blocked waiting on a repo. Creation is not staged behind the design gate —
*slices* are gated (§4), repositories are not.

```sh
ORG=TheAfricanDreamLab

# 1 — org-wide community health files; must exist before the others inherit templates
gh repo create $ORG/.github --public \
  --description "Org-wide issue/PR templates, shared Claude Code skills, community health files"

# 2 — credsync: canonical engine repo, first build target
gh repo create $ORG/credsync --public \
  --description "Offline-first sync engine for apps on unreliable networks" \
  --license apache-2.0 --gitignore Rust

# 3 — the personal fork required by the owner (no --org flag: forks to the authed user)
gh repo fork $ORG/credsync --clone=false --remote=false          # -> ukemeikot/credsync

# 4 — the seam packages: everything else pins these
gh repo create $ORG/dreamlab-contracts  --public --license apache-2.0 --gitignore Node \
  --description "Zod + ts-rest contracts shared by the Dream Lab OS API and clients"
gh repo create $ORG/blueprint-tokens    --public --license apache-2.0 --gitignore Node \
  --description "Founder's Blueprint design tokens for web, React Native, and Claude Design"

# 5 — the four standalone deployables
gh repo create $ORG/dreamlab-api    --public --license apache-2.0 --gitignore Node \
  --description "Dream Lab OS backend — NestJS modular monolith with transactional outbox"
gh repo create $ORG/dreamlab-web    --public --license apache-2.0 --gitignore Node \
  --description "Dream Lab OS web app — student, faculty, mentor, admin, alumni surfaces"
gh repo create $ORG/dreamlab-mobile --public --license apache-2.0 --gitignore Node \
  --description "Dream Lab OS student mobile app — offline-first Expo client"
gh repo create $ORG/dreamlab-site   --public --license apache-2.0 --gitignore Node \
  --description "Dream Lab OS public site and application flow — per-tenant, edge-cached"

# 6 — the one private repo
gh repo create $ORG/dreamlab-infra --private --license apache-2.0 \
  --description "Deployment stacks, Cloudflare config, secrets manifests, restore runbooks"
```

**Status: executed 25 August 2026.** All nine repos exist in the org and `ukemeikot/credsync` is a
verified fork of `TheAfricanDreamLab/credsync`. Applied to every repo post-creation:

| Setting | Value | Result |
|---|---|---|
| Default branch | `main`, initialised with README + Apache-2.0 LICENSE + gitignore | 9/9 |
| Branch protection | PR required (0 approvals — solo builder), linear history, no force-push, no deletion, conversation resolution required, `enforce_admins=false` | 8/9 — see §3.2 |
| Merge strategy | Squash only; merge commits and rebase disabled; branch auto-deleted on merge; squash title = PR title | 9/9 |
| Labels | `kind:slice` / `kind:bug` / `kind:hardening` + per-repo `area:*` | 9/9 |
| Milestones | M0–M7 (credsync) · P0, P2–P7 (Dream Lab repos) | 9/9 |
| Wikis / repo Projects | Disabled — the org-level Project is the single view (§2.2) | 9/9 |

Milestones deliberately **omit P1 on the Dream Lab repos**: P1 is credSync M0–M5 and lives
entirely in the `credsync` repo (§2.3). An empty P1 in seven repos would misreport the progress bar.

Required status checks are left unset until each repo's CI workflow exists — a required check that
names a non-existent workflow blocks every PR. They are attached by each repo's bootstrap slice,
which is the slice that creates the workflow.

Still to do: link the org Project *Dream Lab OS · P0–P7* (§2.2), which needs `admin:org` scope on
the `gh` token — the current token has `read:org` only.

`gh repo fork` needs the target name free — verified 25 Aug 2026: `ukemeikot/credsync` does not exist.

### 3.2 — Org plan constraint on the merge gates

`TheAfricanDreamLab` is on the **GitHub Free** org plan. That is sufficient for eight of the nine
repos, but it breaks one Playbook assumption:

| Capability | Public repos (8 of 9) | `dreamlab-infra` (private) |
|---|---|---|
| Branch protection / rulesets | ✅ Available | ❌ **Not available on Free** — requires Team |
| Required status checks before merge | ✅ | ❌ |
| GitHub Actions minutes | ✅ Unlimited | 2,000 min/month shared quota |

Playbook §5 states "the gates are the reviewer" and makes weakening a gate the one forbidden move.
On the private infra repo that rule cannot be *enforced* by GitHub on the Free plan — it can only
be observed by discipline.

**Decision (owner, 25 Aug 2026): stay on the Free plan.** Confirmed empirically during repo
creation — applying branch protection to `dreamlab-infra` returns:

> `Upgrade to GitHub Pro or make this repository public to enable this feature.`

All eight public repos took protection cleanly. The private infra repo is therefore the one place
where the merge gate is a convention rather than a mechanism. The compensating control, built as
part of **INF-1**:

- A committed `.githooks/pre-push` in `dreamlab-infra` that runs the same checks the CI workflow
  runs and refuses the push on failure; `core.hooksPath` set by the repo's bootstrap script.
- The CI workflow still runs on every push and PR — results are advisory rather than blocking, so
  a red run is visible even though GitHub will not stop the merge.
- `dreamlab-infra` holds **no application logic**: compose files, Cloudflare config, secret
  *manifests* (names and destinations, never values), and runbooks. The blast radius of an
  unreviewed merge is a deploy config, not product code.
- Actions minutes on private repos draw from the org's 2,000 min/month Free quota. Infra workflows
  are kept short and run on push to `main` plus PRs only — no matrix builds, no scheduled jobs.

Revisit if either becomes true: infra CI approaches the monthly minute quota, or a second person
gains write access to the org. Both make the ~$4/month Team seat the cheaper option.

`dreamlab-api` retains the modular-monolith structure from Plan §6.1 — hard module boundaries
(Tenancy, Admissions, Learning, Scoring, Community, Meetings, Billing, Notifications) enforced by
lint rules, communicating through the transactional outbox. Splitting the *clients* out does not
split the *backend* into services; that stays a Plan §6.1 decision, unchanged.

### Per-artefact licence obligations — CI-enforced in every repo

- Every Rust crate: `license = "Apache-2.0"` in `Cargo.toml`, plus `LICENSE` and `NOTICE` at crate root.
- Every npm package: `"license": "Apache-2.0"` in `package.json`, plus `LICENSE` at package root.
- Every internal package inside `dreamlab-api` (`tenant-core`, `meet-kit`, `notify-hub`,
  `media-pipeline`, `ai-eval`): same, so extraction to its own repo is a `git mv`, never a
  licensing exercise.
- Root `NOTICE` names the African Dream Network as copyright holder.
- `cargo-deny` (Rust) and `license-checker` (npm) fail the build on any dependency outside the
  allowlist: Apache-2.0, MIT, BSD-2/3, ISC, Unicode-3.0, Zlib. Copyleft is a blocking defect.

### Authorship

Commits and PRs are authored as Ukeme only. No co-author trailers, no tool attribution in PR
bodies, and no third party listed in `NOTICE`, `AUTHORS`, or package metadata.

### Trademark carve-out

Apache-2.0 §6 grants no trademark rights. Dream Score™, DBPI™, and DSI™ remain African Dream
Network marks regardless of code licence; the `NOTICE` file states this explicitly. Plan §18's
trademark filings are unaffected by open-sourcing the code.

---

## 4. Sequencing & gates

```
NOW ──────────────────────────────────────────────────────────────────────────
  Pre-flight (no issues) ── all 9 repos created in TheAfricanDreamLab (§3.1)
                            + ukemeikot/credsync fork · rustup · registry tokens
       │
       ├── credsync #0 ──> CS-1 … CS-32   (M0–M7)        ◀ STARTS IMMEDIATELY
       │                                                    no design dependency
       │
       └── P0 seam-first ──> CT-1, BT-1 ──> API-1, WEB-1, MOB-1
                                   │              │
                                   │        API-2 … API-6, INF-1   (P0)
                                   │              │
                             ╔═════▼══════════════▼═══════════════════════════╗
                             ║  DESIGN GATE                                   ║
                             ║  Claude Design → pull designs →                ║
                             ║  functional + technical requirements →         ║
                             ║  blueprint-tokens v1 derived from design →     ║
                             ║  P2+ slices written                            ║
                             ╚════════════════════════════════════════════════╝
                                   │
                             P2  LMS core (api + web) + meetings
                                   │  └─ needs credsync M3 (CS-19) for sync surface
                             P3  Mobile app
                                   │  └─ needs credsync M5 (CS-28) on a real device
                             P4  Score + Portfolio + Graduation
                             P5  Residency + Mentorship
                             P6  Admissions + public site   ── before Deadline A
                             P7  Command center, billing, hardening
```

**Four hard gates:**

1. **Seam gate.** No `api`, `web`, or `mobile` slice is written before `dreamlab-contracts` and
   `blueprint-tokens` publish 0.0.1. With standalone repos these are the critical path, not a
   side-quest.
2. **Design gate.** No slice that renders a screen is written before the Claude Design output is
   pulled and converted to functional + technical requirements. P0's schema, auth, tenancy, seed,
   and deploy slices clear this gate and proceed now.
3. **credSync M3 gate.** The mobile sync integration cannot start until `credsyncd` is deployable
   (CS-17 / CS-19).
4. **credSync M5 gate.** The mobile offline write surface depends on `@credsync/react-native`
   (CS-27 / CS-28) proven on a real device.

**Per Playbook §1, unchanged:** planning sessions produce issues; build sessions consume one issue
each. Scope discovered mid-slice becomes a new issue, never an expanded slice.

---

## 5. credsync backlog — 32 slices (M0–M7)

Playbook §7 gives 24. Seven are added or split below: five were oversized against the Playbook's
own "≤5 checkboxes" rule (§3), and four cover requirements stated in Design v2.1 that had no
slice. Additions marked **[NEW]**, splits marked **[SPLIT]**.

Every issue: exactly one `area:*`, exactly one `kind:*`, one milestone. Order within the milestone
is the priority; the top open issue is always next.

### M0 — Spec & protocol crate

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| #0 | Bootstrap the delivery system | CLAUDE.md with §4.1 non-negotiables · `.claude/skills/` (rust-sans-io, credsync-protocol, credsync-sim, rust-ffi-bindings, gh-issue-flow) · `/docs` markdown of all four planning documents · issue + PR templates · labels + milestones M0–M7 · backlog seed script · root LICENSE + NOTICE | gh-issue-flow |
| CS-1 | Cargo workspace + crate scaffolds + CI skeleton | `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` green on empty crates · six crates scaffolded per Design §9 · every crate carries `license = "Apache-2.0"` + LICENSE + NOTICE · `cargo-deny` licence allowlist gate green | rust-sans-io |
| CS-2 | Write `spec.md` v1 (≤4 pages) | Committed at `/docs/spec.md` · every rule in Design §5–§6 appears exactly once · page ceiling asserted by CI · §5.1 checksums, §5.2 digests, §5.3 wire discipline, §6 three conflict classes all present | credsync-protocol |
| CS-3 | Protocol types + canonical JSON codec | `proptest` encode/decode round-trip green for every wire type (Scope, EntityRegistration, Change, PullRequest/Response, Command, PushRequest/Response, ConflictClass) · explicit size limit on every field · canonical encoding byte-stable | credsync-protocol |
| CS-4 | Batch checksums + order-independent scope digest | xxh3 batch checksum over the canonical encoding · digest property tests: order-independence, tombstone handling, `row_version` sensitivity · **benchmark xxh3 vs BLAKE3 on target-class hardware; record the verdict in spec.md** (closes Design §12 Q2) | credsync-protocol |
| CS-5 | Golden fixtures + name reservation | Fixtures decode on a clean checkout · `credsync` + `cred-sync` published 0.0.1 to crates.io · `credsync`, `cred-sync`, `@credsync/protocol` published 0.0.1 to npm · every placeholder carries the Apache-2.0 licence field | credsync-protocol |

### M1 — Core state machine

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-6 | Core skeleton: four traits + event/effect surface | `Clock`, `Entropy`, `Storage`, `Transport` defined, none implemented · `Event`/`Effect` enums · engine struct compiles under `#![forbid(unsafe_code)]` · **`cargo tree` asserts zero I/O crates in the graph** — no tokio, no reqwest, no `std::time` inside core | rust-sans-io |
| CS-7 | Pull apply path | Cursor walk · strict seq-ordered apply · gap rejection · tombstone handling · digest maintained incrementally on apply · cursor persisted in the same storage transaction as the applied rows | rust-sans-io |
| CS-8 | Outbox | Enqueue · push batching by compressed byte budget · per-command result handling · dead-letter states · **property test: no acknowledged command is ever lost across arbitrary result sequences** | rust-sans-io |
| CS-9 | Conflict application per entity class | Server-authoritative (command targeting rejected by the entity registry, not by convention) · LWW-per-field decided by server `row_version` with `client_ts` as tiebreaker hint only · append-only · **recovered draft stored locally on LWW loss** · one class-conformance test per class | rust-sans-io |
| CS-10 | **[NEW]** Miri + property-test CI gates | `cargo miri test` green on `credsync-core` and `credsync-protocol` in CI · proptest case counts configured for PR vs nightly · gate required for merge | rust-sans-io |

*Why CS-10 exists:* Playbook §5 lists Miri as a merge gate, but CS-1's CI skeleton is
fmt/clippy/test only and no slice wires it. The gate must land the moment there is core code to check.

### M2 — Simulator

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-11 | Simulator v0 | Fake `Clock`/`Entropy`/`Storage`/`Transport` · seeded scheduler · full Design §7.1 fault menu (drop every Nth, duplicate, reorder, sever mid-batch, 90s flap, kill between txn and ack, partial-visibility storage failure, ±3-day clock skew, cold-cache restart, malformed and truncated bytes) · **1,000-seed batch deterministic: same seed produces a byte-identical trace** · seed printed on failure | credsync-sim |
| CS-12 | Invariants, checked continuously | Convergence (digest equality at quiescence) · durability · idempotency · cursor monotonicity · no-loss · **policy conformance generated per entity class** · all asserted during runs, not only at quiescence | credsync-sim |
| CS-13 | Planted-bug drill | Three seeded bugs on a branch, one per class (ordering, dedupe, conflict) · harness catches all three · each replays exactly from its seed · **drill procedure documented in the `credsync-sim` skill** | credsync-sim |

### M3 — Server

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-14 | Postgres schema + pull pagination | `sync_changes` + `sync_command_results` migrations · cursor pagination · **batches capped by compressed byte budget (100 KB default), never row count** · integration test against real Postgres | — |
| CS-15 | Command dedupe + result store + checksum rejection | Replay returns the recorded outcome without re-applying · **mutated replay rejected as invalid, not deduped as success** (Design §5.1) · dedupe hit-rate metric exposed | — |
| CS-16 | Host command forwarding | Command POST to the host's registered endpoint · applied / rejected / superseded recorded against the command id · reference in-process host in tests · **no domain logic inside credsyncd**, asserted by the review checklist | — |
| CS-17 | `credsyncd` binary + scope-token validation | axum wiring · short-lived JWT signature and scope-claim validation · pull can never cross scopes regardless of client input · **server-side scope blocklist for immediate revocation** (Design §8) | — |
| CS-18 | **[SPLIT]** Backpressure + server-in-simulator | Backpressure under load · **the real server logic runs inside the simulator alongside the real core** in one seeded run · e2e sim scenario green | credsync-sim |
| CS-19 | **[NEW]** Initial-snapshot bootstrap endpoint | Per-scope bootstrap from seq 0, paginated and byte-budgeted · new-device bootstrap scenario in the simulator · **decision recorded in spec.md: paginated pull in v1, signed snapshot file deferred** (closes Design §12 Q3) | credsync-protocol |

*Why CS-19 exists:* Plan §7.4 requires a bootstrap endpoint for new devices, and CS-22's
tainted-scope re-bootstrap has nothing to call without it. The self-heal path is unbuildable until
this lands.

### M4 — Migrations & upgrade paths

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-20 | Schema-version migrations + outbox forward-migration | Client applies registered up-migrations to local rows · queued outbox commands authored under an old schema migrate forward before push, **never dropped** · sim scenario: v-old client against v-new server through the N-1 window | rust-sans-io |
| CS-21 | **[SPLIT]** Forced-upgrade envelope + N-1 window | Server speaks N and N-1 · below N-1 returns 426 with an envelope the client core understands · client surfaces the upgrade prompt and **queues rather than drops** · sim scenario green | credsync-protocol |
| CS-22 | **[SPLIT]** Divergence self-heal | Digest mismatch after apply → scope marked tainted → re-bootstrap from a fresh snapshot → outbox replayed → telemetry event carrying **both digests** · sim scenario forces divergence and asserts recovery | credsync-sim |

### M5 — FFI & React Native

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-23 | `credsync-ffi`: UniFFI surface | UniFFI annotations over the event-in / effects-out surface only · Kotlin and Swift bindings generate and compile · `unsafe` confined to generated scaffolding | rust-ffi-bindings |
| CS-24 | **[SPLIT]** rusqlite Storage adapter | SQLite compiled into the core cdylib (no host SQLite version roulette) · local schema: mirrored entity tables plus `credsync_outbox`, `credsync_cursors`, `credsync_meta` · **adapter passes the full conformance suite** | rust-ffi-bindings |
| CS-25 | **[SPLIT]** Binding build pipeline + size budget | xcframework and AAR built by script from day one · **CI builds aarch64 iOS + Android artefacts on every PR touching core/ffi** · `.so` size budget ≤ 3 MB per ABI enforced per commit · arm64 + armv7 only, LTO + strip | rust-ffi-bindings |
| CS-26 | **[NEW]** SQLCipher at-rest encryption | Feature-flagged SQLCipher storage · host supplies the key from its PIN/biometric gate · conformance passes with the flag on and off · size budget still met | rust-ffi-bindings |
| CS-27 | `@credsync/react-native` package | Generated Turbo Module + TS API sugar (subscribe, status, dead-letter surface) · published with the Apache-2.0 licence field · **fallback path documented** should `uniffi-bindgen-react-native` block (Design §11) | rust-ffi-bindings |
| CS-28 | **[SPLIT]** Expo example app + device demo | Reference Expo app syncing lessons / submissions / reflections · **airplane-mode demo on a real low-end Android device: write offline, kill the app, reconnect, converge, digests match** · demo recorded | rust-ffi-bindings |

*Why CS-26 exists:* Design §8 makes SQLCipher a stated requirement driven by Dream Lab's
shared-phone PIN gate (Plan §11), but Playbook §7 has no slice for it. Without it, the mobile
"locked store behind PIN" slice has no engine support.

### M6 — Hardening

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-29 | cargo-fuzz targets on all wire decoders | Pull batches, command envelopes, snapshots · 60s smoke per decoder on PR · overnight run clean · any crash becomes an issue carrying its input | credsync-sim |
| CS-30 | Coverage hunting | Explored-state statistics report · fault distributions tuned against it · **planted-bug drill repeated with a new bug class** · report reviewed and committed | credsync-sim |
| CS-31 | Telemetry surface + docs + quickstart | Client telemetry: cycle outcomes, retry depth, dead-letter counts, divergence events with digest pairs, migration events · `credsyncd` exposes per-scope lag, reject rate by reason, dedupe hit rate · **quickstart executed cold by Emanamfon; every friction point becomes an issue** | — |

### M7 — Release

| # | Slice | Definition of done | Skills |
|---|---|---|---|
| CS-32 | v0.1.0 + Dream Lab integration handshake | Tag pushed · all crates and npm packages published at 0.1.0 with correct licence metadata · reference-server pattern wired into `dreamlab-api`'s Learning module for lessons / submissions / reflections · **the Dream Lab student flow passes the conformance scenarios end to end** | — |

---

## 6. Dream Lab backlog

### P0 Foundations — 12 slices, no design dependency, starts now

Playbook §8's DL-1 ("Bootstrap Turborepo") becomes five bootstrap slices, one per repo. The seam
repos go first; the three consumers then prove the seam with a hello-screen each, at P0, when
proving it is cheap.

| # | Repo | Slice | Definition of done |
|---|---|---|---|
| CT-1 | contracts | Bootstrap `dreamlab-contracts` | Zod + ts-rest scaffold · CI (lint, tsc, test) · LICENSE + Apache-2.0 in package.json · **published `@dreamlab/contracts@0.0.1`** · version-compatibility check job wired |
| BT-1 | tokens | Bootstrap `blueprint-tokens` | Founder's Blueprint placeholder tokens as plain TS (navy `#0A0F1E`, gold `#D8A84E`, jade `#3CB795`; Space Grotesk / Inter / JetBrains Mono) · web CSS + RN style exports · LICENSE · **published `@dreamlab/blueprint-tokens@0.0.1`** |
| API-1 | api | Bootstrap `dreamlab-api` | NestJS + Fastify · **module-boundary lint rules per Plan §6.1** · CI skeleton · LICENSE + licence-allowlist gate · pins `@dreamlab/contracts@0.0.1` exactly · health endpoint served from a contract |
| WEB-1 | web | Bootstrap `dreamlab-web` | Vite + React · CI skeleton · LICENSE · **consumes both seam packages**; hello-screen renders blueprint tokens and calls the contract-typed health endpoint |
| MOB-1 | mobile | Bootstrap `dreamlab-mobile` | Expo · CI skeleton · LICENSE · **consumes both seam packages**; hello-screen renders blueprint tokens in RN styles and calls the contract-typed health endpoint |
| API-2 | api | Drizzle schema: tenancy & identity + RLS | institutions, programs, cohorts, users, institution_members · migration applies · **cross-tenant leak test fails loudly** |
| API-3 | api | Auth: register / login / OTP, JWT + refresh in Redis, RBAC guards | Contract tests for every auth flow · role-guard matrix tested against Plan §14 |
| API-4 | api | Subdomain tenant resolution + tenant-scoped db wrapper | **An unscoped query attempt fails a lint or test** · `dreamlab.<domain>` resolves in dev |
| API-5 | api | Curriculum seed from the academic spec | 13 courses with credits · 15-week calendar · 7 residency stage definitions · 10 graduation requirements · seed idempotent · **totals assert 100 credits, 25/20/10/45 phase weights, residency split 5/5/7/10/6/5/7** |
| API-6 | api | R2 presigned upload / download | Keys prefixed `{institution_id}/` · presign round-trip test · size and content-type limits enforced |
| INF-1 | infra | Dokploy stack + CI/CD to Hetzner | Staging deploys on merge to main for api / web · Cloudflare wildcard DNS · secrets manifest · **all four client repos deploy independently** |
| INF-2 | infra | Backup + first restore drill | Nightly `pg_dump` + WAL archiving to R2 · **documented restore executed before any real data exists**, RTO recorded (Plan §12) |

### P2 onward — written after the design gate

Playbook §8's DL-7…DL-25 are the template, remapped per §2.3 and redistributed across the api /
web / mobile repos. Per the agreed sequence, those slices are written **after** the Claude Design
output is pulled and converted to functional + technical requirements.

Five gaps in the Playbook backlog to close at that point:

1. **`blueprint-tokens` v1 derivation** — BT-1 seeds placeholders; a gate-clearing slice replaces
   them with tokens extracted from the actual Claude Design output, and bumps to 1.0.0.
2. **`media-pipeline` own slice** — DL-9, DL-10, and DL-23 all consume it; it has no slice of its
   own. Chunked resumable upload, image variants, Opus transcode, signed pack manifests.
3. **`notify-hub` own slice** — DL-20 consumes it; it has no slice of its own. Channel routing,
   push → WhatsApp → SMS → email fallback chain, per-tenant metering.
4. **`harmattan` budget gate slice** — Playbook §5 makes NG-3G-p75 budgets a merge gate and
   `ng-network-budget` a skill, but no slice builds the checker. It must exist in `web` and
   `mobile` CI before the first screen ships.
5. **Contract-drift nightly job** — new obligation created by the standalone split (§2.1).

P4–P7 batches (residency polish, admissions, command center, billing, hardening) are written at
the P3 boundary, as Playbook §8 instructs.

---

## 7. Pre-flight — before the first issue

Environment setup on the build machine. Not slices.

| Item | Status | Action |
|---|---|---|
| GitHub org | `TheAfricanDreamLab`, owner is `admin`, currently empty | **Home for all nine repos.** Create them in one pass per §3.1, then fork credsync to `ukemeikot` |
| Rust toolchain | **Missing** — no `cargo`, no `rustc` | Install rustup stable with `rustfmt`, `clippy`; add `miri` (nightly component), `cargo-deny`, `cargo-fuzz`; add aarch64 iOS/Android targets at M5 |
| Node 24 / pnpm 10 | Present | — |
| Docker | Present | Needed for Postgres integration tests at CS-14 |
| `gh` scopes | `gist, read:org, repo, workflow` | Sufficient to create repos and issues. `admin:org` needed only for org-level rulesets or the org Project |
| crates.io token | Not set | Needed at CS-5 |
| npm token + `@credsync` and `@dreamlab` scopes | Not set | Needed at CS-5 and CT-1 |
| Cohort 04 dates (Deadlines A & B) | **Unknown** | Plan §18 — the only inputs that can reorder P4–P7. Blocks neither credsync nor P0 |
| Termii WhatsApp Business onboarding | Not started | Plan §18 warns of weeks of lead time — start during P0, not P3 |
| LiveKit Cloud account | Not started | Confirm Ship-tier egress pricing for 90-minute composite recordings before P2 |

---

## 8. Slice count

| Repo | Milestone range | Slices | Writable today |
|---|---|---|---|
| credsync | M0–M7 | 32 (#0 + CS-1…CS-32) | **All** — no design dependency |
| contracts / tokens / api / web / mobile / infra | P0 | 12 + issue zeros | **All** — no design dependency |
| api / web / mobile | P2–P5 | ~25 + 5 gap slices | After the design gate |
| api / web / mobile / site | P6–P7 | TBD | At the P3 boundary |

**Roughly 44 issues are writable today.** The remainder waits on Claude Design.
