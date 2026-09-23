---
name: rust-sans-io
description: The sans-IO state-machine pattern for credsync-core - the four traits, how to add a state transition with its test, and the ban list. Use when touching credsync-core, adding an Event or Effect, implementing pull/outbox/conflict logic, or reviewing anything that might sneak I/O into the core.
---

# Sans-IO core

The core is a state machine that never touches the real world. Feed it events, drain effects.
Nothing inside it reads a clock, generates randomness, opens a socket, or writes a file.

This is not a style preference. It is the single property that makes deterministic simulation
possible — and DST is how this project earns the right to claim it never loses a write.

## The shape

The engine **owns all four implementations** and calls them directly (D-037, settled at CS-6).

```rust
let mut engine = Engine::new(clock, entropy, storage, transport);

// Events go in.
engine.handle(Event::Tick { now });
engine.handle(Event::TransportResponse { id, result });

// Effects come out — only what the four traits cannot express.
while let Some(effect) = engine.next_effect() {
    match effect {
        Effect::ScheduleRetry { at } => timer.set(at),
        Effect::Emit(telemetry)      => host.emit(telemetry),
        _ => {}                       // Effect is #[non_exhaustive] for callers
    }
}
```

**There is no `Effect::Send` or `Effect::Persist`** — an earlier draft of this skill showed
those, and it was wrong. Where the boundary falls depends on whether a trait answers immediately:

| Trait | Called by | Result arrives as | Why |
|---|---|---|---|
| `Clock` | engine | return value | Immediate. |
| `Entropy` | engine | return value | Immediate. |
| `Storage` | engine | return value | `transact` is synchronous. Routing its outcome back through the event queue would park a half-finished apply across a round trip — and `spec.md` §4 needs rows and cursor to commit together. |
| `Transport` | engine | `Event::TransportResponse` | A request handed to a network that routinely does not answer cannot be awaited. |

The engine decides *what* must happen and, for everything with an immediate answer, does it. In
production the four are real implementations; in the simulator they are seeded fakes. **The same
core bytes run in both worlds** — that is the entire trick.

## The four traits

```rust
trait Clock     { fn now(&self) -> Timestamp; }
trait Entropy   { fn fill(&mut self, buf: &mut [u8]); }   // UUIDv7, jitter
trait Storage   { fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError>; }
trait Transport { fn enqueue(&mut self, req: WireRequest) -> Result<RequestId, TransportError>; }
```

They are **parameters of the core, never dependencies of it** — declared in `credsync-core`,
implemented only outside it. Time enters as `Event::Tick`, randomness as bytes from `Entropy`,
never as an ambient call.

**None of them may gain a `Send` or `Sync` bound.** One engine, one thread. Hermes is
single-threaded and `rusqlite::Connection` is not `Sync`, so a `Send` bound would push every
binding into wrapping its database handle in a mutex to satisfy a constraint the design never
had. `credsync-core/tests/single_threaded.rs` builds an engine from four `!Send` parts, so adding
such a bound stops compiling.

## Ban list

Any of these inside `credsync-core` or `credsync-protocol` is a review-blocking defect. CI greps
for them.

| Banned | Instead |
|---|---|
| `std::time::Instant::now()`, `SystemTime::now()` | `Clock::now()`, injected |
| `thread::sleep`, any blocking wait | Return `Effect::ScheduleRetry(at)` |
| `rand::*`, `getrandom` | `Entropy::fill()` |
| `tokio`, `async`/`await`, any executor | The core is synchronous. Async lives in `credsyncd` |
| `reqwest`, `hyper`, any HTTP client | Return `Effect::Send(req)` |
| `std::fs`, `rusqlite` | Return `Effect::Persist(ops)` |
| `unsafe` | Forbidden by attribute |
| `HashMap` iteration order in anything hashed or ordered | `BTreeMap`, or sort explicitly |

That last row is subtle and has bitten real projects: `HashMap`'s iteration order is randomised
per process, so a digest or a serialisation built by iterating one is nondeterministic even
though nothing obviously "does I/O". If order can affect output, use an ordered map.

Verify with:

```sh
./scripts/check-sans-io.sh         # greps the SOURCE of core and protocol
cargo tree -p credsync-core        # no tokio, no reqwest, no rusqlite
cargo test -p credsync-core        # runs without a runtime
```

The first two are complements, not duplicates, and the distinction matters (D-038). `cargo tree`
watches what the crate *depends on*; it cannot see `SystemTime::now()`, which needs no dependency,
touches no manifest, compiles cleanly, and leaves every test green while deterministic replay is
silently gone. `check-sans-io.sh` watches what the crate *writes*.

It strips comment-only lines before matching, so the prose explaining a ban does not trip the
gate — but a trailing comment on a line of code is **not** stripped. Put `// see Instant::now` on
its own line.

When you add a row to the ban list above, add the pattern to `scripts/check-sans-io.sh` too. A
ban list that only exists in a document is a suggestion.

## Adding a state transition

Do all five in **one** PR. A transition without its test is not done.

1. **Add the `Event` variant.** What the outside world tells the engine.
2. **Add the `Effect` variant** if the engine needs something new performed.
3. **Implement the transition** in the engine's `handle`. Exhaustive `match` — never a
   catch-all `_ =>` arm, which is how a new variant gets silently ignored.
4. **Unit-test the transition directly.** Construct the state, feed the event, assert the
   effects. No simulator needed for this layer.
5. **Add the invariant** to `credsync-sim` if the transition makes a claim about behaviour under
   fault — and **break it once** to confirm the invariant catches it before trusting it.

## Staging a transaction: read your own writes

Anything that batches several changes into one `Storage::transact` call must track what the batch
has already decided, because **none of it has committed yet**. `Storage::row_version` answers from
the database, which still holds the pre-batch state, so a lookup for a row an earlier change in
the same batch created or deleted returns the wrong answer.

This is not hypothetical. It shipped in the first draft of CS-7 and the property test caught it on
a two-change sequence (D-041): a row upserted and then tombstoned inside one batch was added to the
scope digest and never subtracted, because the tombstone looked up a row storage had never heard
of. The client would then disagree with the server on every subsequent pull and re-bootstrap the
scope forever — the divergence detector firing on damage it had caused itself.

The fix is an in-batch overlay consulted before storage:

```rust
let current = match staged.get(&key) {
    Some(pending) => *pending,           // this batch already decided
    None => storage.row_version(e, i)?,  // fall back to committed state
};
```

Any future slice that stages multi-op transactions — the outbox at CS-8, migrations at CS-20 —
needs the same discipline. If your staging logic reads state that your own ops are about to
change, it must read the overlay first.

## Testing this layer

The core is the easiest thing in the repo to test well, precisely because it is pure: state in,
effects out, no setup.

- **Prefer property tests.** "Applying any valid change sequence yields a digest equal to a
  from-scratch recomputation" is a property. Three hand-written examples are not.
- **Test the interrupted path.** Every transition that persists must be correct when the process
  dies immediately after `Effect::Persist` is emitted but before the caller acknowledges it.
  This is the single most common source of real sync corruption.
- **Assert on effects, not on internals.** A test that reaches into private state will break on
  every refactor and prove nothing about behaviour.
- **Determinism is testable.** Run the same event sequence twice; the effect stream must be
  byte-identical. Make this a test, not an assumption.

## Why this shape is also the easy shape

Design §3.1 is honest that this is a first substantial Rust project. Sans-IO helps: the core is
plain synchronous Rust with no lifetimes gymnastics, no async, no `Pin`, no `Send`/`Sync` bounds.
Async enters only at `credsyncd` (M3), and FFI only at M5. If you find yourself fighting the
borrow checker in the core, the design has probably drifted — the core should be boring.
