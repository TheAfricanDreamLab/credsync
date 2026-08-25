# credSync

**An offline-first data synchronization engine for apps on unreliable networks.**

Not a credentials tool. credSync keeps a client's local database in step with a server over
connections that drop, flap, duplicate, reorder, and lie about the time — the 2G-worst,
3G-typical, intermittent-always reality its first users live in.

[![licence](https://img.shields.io/badge/licence-Apache--2.0-blue.svg)](LICENSE)
[![status](https://img.shields.io/badge/status-pre--alpha-orange.svg)](#status)
[![rust](https://img.shields.io/badge/rust-1.98.0-orange.svg)](rust-toolchain.toml)

---

## What it does

**State flows down. Commands flow up.**

```
                    ┌──────────────────────────────┐
   pull  ◀──────────│  append-only change log      │   ordered snapshots,
                    │  (seq, scope, entity, …)     │   never diffs
                    └──────────────────────────────┘
   ┌─────────┐                                        ┌──────────────┐
   │ client  │                                        │ your backend │
   │  core   │──────▶ commands ──────▶ credsyncd ────▶│ validates &  │
   └─────────┘        (UUIDv7,          (dedupe,      │ applies      │
                       idempotent)       forward)     └──────────────┘
```

The server is authoritative. Clients never write rows directly — they submit **domain commands**
that your backend validates against its own business rules. State comes back as ordered
full-row snapshots walked from a cursor. There is no automatic merge of business data, ever.

## Why it exists

Every offline-first engine that earned production trust converged on roughly this shape, and the
ones that tried harder things retreated. ElectricSQL abandoned CRDT active-active for a read-path
engine. Replicache pioneered server-authoritative mutators, then was sunset for Zero. Triplit
folded. Atlas Device Sync was deprecated.

The lesson credSync takes from that history is not a technique but a posture: **never build a
product's offline story on a vendor's continued existence.** PowerSync validated this exact shape
with a Rust client core, but its service layer is source-available, not open. That gap is
credSync's opening.

## Design commitments

| | |
|---|---|
| **Never loses an acknowledged write** | The property everything else serves. Proven by simulation, not asserted |
| **Server-authoritative, command-based** | Your backend keeps its business rules; credSync never guesses |
| **Sans-IO deterministic core** | No clocks, no entropy, no I/O inside the state machine — so the whole engine runs inside a seeded simulator |
| **Explainable conflicts** | Three declared classes. A losing draft returns to the device as a recovered draft; silent loss is a protocol violation |
| **Backend-agnostic** | Any stack that can expose one HTTP endpoint and write two tables can adopt it |
| **Four-page protocol** | A hard ceiling. If the spec outgrows it, scope has crept |

**Not goals:** CRDTs, peer-to-peer sync, partial-field diffs, realtime transport, multi-device
merge beyond per-field last-write-wins. Each was considered and declined.

## How it earns trust

The engine is verified by **deterministic simulation testing**, in the tradition of FoundationDB
and TigerBeetle. Simulated devices and a simulated server run the *real* core and *real* server
logic against seeded fakes for clock, entropy, storage, and transport.

Every packet delay, drop, duplication, reorder, process kill, and ±3-day clock skew flows from one
RNG seed — so simulated weeks of flaky-network device life run in seconds, and **any failure
replays exactly from its seed.** A bug report here is one integer.

Invariants are checked continuously, not just at quiescence: convergence, durability, idempotency,
cursor monotonicity, no-loss, and per-class policy conformance.

Behind that sits a scope **state digest** — an order-independent hash both sides maintain and
compare on every pull. A mismatch means silent divergence, the class of bug that testing missed
and users never report until trust is gone. The client re-bootstraps automatically and emits both
digests as telemetry. Self-healing plus a signal, instead of quiet rot.

## Status

**Pre-alpha. Not yet usable.** The protocol specification is being written; no release exists.

Progress is tracked as [milestones M0–M7](https://github.com/TheAfricanDreamLab/credsync/milestones):

| | | |
|---|---|---|
| M0 | Spec & protocol crate | ← current |
| M1 | Core state machine | |
| M2 | Simulator | |
| M3 | Server | |
| M4 | Migrations & upgrade paths | |
| M5 | FFI & React Native | |
| M6 | Hardening | |
| M7 | v0.1.0 | |

Watch the repo if you want the v0.1.0 release.

## Architecture

One Rust core, many targets — the architecture Mozilla proved at hundreds-of-millions scale for
Firefox Sync.

| Crate | What it is |
|---|---|
| `credsync-protocol` | Wire types, canonical codec, checksums, digests, versioning rules, the spec |
| `credsync-core` | The sans-IO state machine: sync loop, outbox, cursors, migrations, conflicts |
| `credsync-server` / `credsyncd` | Pull pagination, dedupe, scope tokens, host forwarding; axum reference binary |
| `credsync-sim` | Deterministic harness: fault scheduler, invariant checkers, seed replay |
| `credsync-ffi` | UniFFI surface over the core's event/effect API |
| `@credsync/react-native` | Turbo Module + TypeScript API |

The core is `#![forbid(unsafe_code)]`. `unsafe` appears only in generated FFI scaffolding.

## Who it's for

Built for [Dream Lab OS](https://github.com/TheAfricanDreamLab), whose students use low-end Android
phones on congested Nigerian 3G with a data budget and unreliable power. That constraint shaped
every decision here.

If your users are on good connections, you probably do not need this. If some of them are not,
credSync is built for them rather than adapted to them.

## Documentation

| | |
|---|---|
| [Protocol spec](docs/spec.md) | The wire format. Four pages, hard ceiling *(from CS-2)* |
| [Design document](docs/credsync-design-v2.1.md) | Prior art, language decision, architecture, robustness |
| [Decision register](docs/DECISIONS.md) | Every binding decision, with its source |
| [Contributing](CLAUDE.md) | How work happens here |

## Contributing

Every change starts as an issue and ends as a merged PR that closes it. Issues carry an executable
definition of done — a command that passes or a behaviour a test proves, never "works correctly".

Two rules worth knowing before opening a PR:

- **Every behaviour claim ships with the test that proves it**, in the same PR.
- **Never weaken a fault distribution or skip a seed batch to make CI pass.** The correct response
  to a red gate is a fix, or a bug issue carrying its seed.

See [CLAUDE.md](CLAUDE.md) for the full working agreement.

## Licence

[Apache-2.0](LICENSE). See [NOTICE](NOTICE) — the licence covers the code; the African Dream
Network's trademarks are not granted with it.
