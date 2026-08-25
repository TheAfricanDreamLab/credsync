---
name: credsync-protocol
description: The wire-change procedure for credSync - spec.md first, then types, codec, fixtures, property tests, all in one PR. Covers checksum and digest rules, the four-page ceiling, and protocol/schema versioning with the N-1 window. Use when touching credsync-protocol, changing any wire type, or implementing pull/push/bootstrap endpoints.
---

# Changing the wire

## The procedure — one PR, this order

A wire change split across PRs leaves the repo in a state where the spec, the types, and the
fixtures disagree. That state is undetectable by CI and lethal to a protocol. So:

1. **`docs/spec.md` first.** If you cannot write the rule in a sentence, you do not understand
   it well enough to implement it.
2. **Types second.** Rust types in `credsync-protocol` matching the spec exactly.
3. **Codec third.** Canonical encoding — same value, same bytes, always.
4. **Fixtures fourth.** A golden file per wire type, committed.
5. **Property tests fifth.** Round-trip for every type.

All five in the same PR. `spec.md` is law; the code is its implementation, not its source.

## The four-page ceiling

Hard limit, checked in CI. If the spec grows past four pages, **scope has crept** — that is what
the ceiling is for. The response is to cut scope, never to raise the ceiling or shrink the font.

## Canonical encoding

Compact JSON in v1, Brotli/gzip on the wire. The codec is isolated so a binary encoding can
arrive as protocol v2 without touching the state machine (D-007).

**Canonical means byte-stable**: the same logical value always encodes to identical bytes. This
is load-bearing — checksums and digests are computed over the encoding, so instability there
produces phantom corruption reports.

- Object keys in a fixed order (sorted, not insertion order)
- No incidental whitespace
- Numbers in one representation only
- **Never iterate a `HashMap` to build encoded output** — its order is randomised per process

Every field has an explicit size limit declared in the spec. Oversize input is **rejected, not
truncated**. Truncation turns a hostile input into a silently wrong value, which is worse.

## Checksums (Design §5.1)

- Every pull batch carries a checksum over its canonical encoding. Corruption on lossy links, in
  flaky proxy caches, or in device flash is caught **before apply**; the batch is refetched,
  never half-applied.
- Every command carries a payload checksum recorded with its dedupe entry. A replay with a
  mutated body is **rejected as a distinct invalid request**, not deduped as a success. Skipping
  this turns the dedupe table into a way to launder tampered commands.

Algorithm choice (xxh3 vs BLAKE3) is **O-001**, decided at CS-4 by benchmark. Until then, do not
hardcode an assumption in more than one place.

## Scope digests (Design §5.2)

An order-independent rolling hash over `(entity, entity_id, row_version)` for all live rows in a
scope. Both sides maintain it; the server returns its digest with every pull; the client compares
after apply.

A mismatch means **silent divergence** — the class of bug that testing missed and users never
report until trust is gone. The client marks the scope tainted, re-bootstraps, replays its
outbox, and emits telemetry carrying *both* digests.

Properties that must hold, and must be property-tested:

- **Order-independent**: any permutation of the same live set yields the same digest
- **Incremental**: applying N changes one at a time equals applying them as a batch
- **Tombstone-correct**: a deleted row leaves the digest as if it had never existed
- **`row_version`-sensitive**: same row at a different version yields a different digest

## Versioning

**Protocol version** — on every request. The server speaks N and N-1. Below N-1: respond `426`
with a forced-upgrade envelope the client core understands. The client then **queues its outbox,
never drops it**, and surfaces an upgrade prompt.

**Schema version** — per entity, in every snapshot. The client applies registered up-migrations
to local rows. Outbox entries record the schema version they were authored under; an upgraded app
migrates queued commands forward before pushing.

The rule underneath both: **a client that has been offline for three weeks must never lose the
work it queued.** Every versioning decision is subordinate to that.

## Testing the wire

The network is adversarial. Test accordingly.

- **Round-trip property test** for every type — `proptest`, not examples.
- **Golden fixtures** catch what round-trips cannot: a round-trip test still passes if you change
  both the encoder and decoder in the same wrong way. Fixtures pin the actual bytes.
- **Fuzz every decoder** (`cargo-fuzz`, CS-29). Decoders must reject malformed and truncated
  input without panicking. A panic in a decoder is a remote crash.
- **Test at the size limits**, not just inside them: at the limit, one byte over, empty, and
  absent.
- **A fixture that needs updating is a signal.** If a change makes a fixture fail, the wire moved.
  Confirm that was intended and that `spec.md` moved with it — do not just regenerate the fixture.
