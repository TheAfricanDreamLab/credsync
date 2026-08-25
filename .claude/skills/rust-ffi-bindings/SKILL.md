---
name: rust-ffi-bindings
description: UniFFI binding patterns for credsync-ffi - the event/effect FFI surface, xcframework and AAR build commands, binding size budget, and the React Native fallback path. Use when working in credsync-ffi, on @credsync/react-native, on the rusqlite or SQLCipher storage adapters, or on the mobile build pipeline. STATUS - provisional until CS-23.
---

# FFI bindings

> **STATUS: PROVISIONAL — drafted at CS-0 from the design documents, not yet validated against a
> real build.**
>
> Nothing below has been run. Treat every command as a starting hypothesis, not a known-good
> recipe. **CS-23 (#24) rewrites this file from actual experience** and removes this banner.
>
> Premature FFI documentation is worse than none, because it reads as tested knowledge. If
> something here contradicts what the toolchain actually does, the toolchain is right — fix this
> file in the same PR that discovers it.

## The surface: small and message-shaped

The FFI boundary exposes the core's event/effect API and nothing else:

```
feed event in  →  drain effects out
```

That is the whole contract. **No object graph crosses the boundary.** No callbacks into Rust
holding references. No shared mutable state.

Two reasons this matters:

- **Every binding stays thin.** Kotlin, Swift, and the RN Turbo Module are all the same two calls.
- **Hermes is single-threaded.** A message-shaped surface keeps React Native's JS engine happy;
  anything requiring cross-thread coordination will fight it.

`unsafe` appears **only** in generated scaffolding. `credsync-core` and `credsync-protocol` keep
`#![forbid(unsafe_code)]`.

## Precedent

Firefox Sync runs one Rust core deployed to Kotlin and Swift via UniFFI at hundreds-of-millions
scale (Design §2). That is the exact architecture here — it is a well-trodden path, not an
experiment. The experimental part is only the React Native generator.

## Expected build shape

*Unverified — confirm at CS-23/CS-25.*

```sh
# Kotlin / Swift binding generation
cargo run --bin uniffi-bindgen -- generate --library <cdylib> --language kotlin --out-dir <dir>
cargo run --bin uniffi-bindgen -- generate --library <cdylib> --language swift  --out-dir <dir>

# Android targets - arm64 + armv7 only (D: size budget)
cargo build --release --target aarch64-linux-android
cargo build --release --target armv7-linux-androideabi

# iOS
cargo build --release --target aarch64-apple-ios
xcodebuild -create-xcframework -library ... -output CredSync.xcframework
```

Scripted in `credsync-ffi` **from day one**, not hand-run. CI builds device artifacts on every PR
touching `core/` or `ffi/`, so breakage is caught at the commit that caused it rather than at M5
integration (Design §11).

## Size budget

**`.so` ≤ 3 MB per ABI**, tracked per commit and enforced in CI. Rust core plus rusqlite plus
SQLCipher adds real megabytes to an APK, and the target user is on a 32 GB phone where
uninstalls follow bloat (Plan risk R7).

Levers, in the order to reach for them: LTO, strip symbols, `panic = "abort"`, `opt-level = "z"`,
drop unused UniFFI features, arm64 + armv7 only.

Track the size **over time**, not just against the ceiling — steady growth is visible long before
it breaches, and is much cheaper to address early.

## React Native: the known risk

`uniffi-bindgen-react-native` self-describes as early-stage; core UniFFI is production-proven but
pre-1.0 (Design §11). This is **O-003**, resolved at CS-27.

- **Preferred path:** generated Turbo Module + TypeScript sugar.
- **Fallback:** mature UniFFI Kotlin/Swift bindings wrapped in a thin hand-written RN native
  module — one file per platform.

**The core is untouched either way.** If the generator blocks, do not redesign the core to suit
it; take the fallback and record the decision in `DECISIONS.md`.

## Storage adapters

- **rusqlite compiled into the core's cdylib** (CS-24). One artifact, no host SQLite version
  roulette.
- Local schema: mirrored entity tables plus `credsync_outbox`, `credsync_cursors`,
  `credsync_meta`.
- **SQLCipher behind a feature flag** (CS-26). The host supplies the key from its PIN/biometric
  gate; credSync never persists the key.

## Testing across the boundary

FFI is where testing gets hard and therefore where it gets skipped. Do not skip it.

- **The adapter proves itself against the conformance suite**, not bespoke tests. Any adapter
  that passes conformance is correct by construction — that is the whole contract, and it is why
  the suite exists.
- **Round-trip through the real boundary** in both Kotlin and Swift, not just through a Rust-side
  mock.
- **Test the kill path on device.** Airplane mode, write, kill the app, reconnect, converge,
  digests match (CS-28). A simulator cannot prove this; only hardware can.
- **Storage errors surface as typed effects, never panics.** A panic across an FFI boundary is
  undefined behaviour, not an exception.
- **Assert the size budget in CI**, not by remembering to check.

## Traps to record here as they are found

*(Empty by design. CS-23 onward fills this from real experience — Hermes quirks, xcframework
signing, AAR packaging, NDK version pinning, symbol stripping surprises. If you hit one, write it
down here in the same PR.)*
