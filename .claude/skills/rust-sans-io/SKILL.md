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

```rust
// Events go in.
engine.handle(Event::TransportResponse { id, bytes });
engine.handle(Event::Tick { now });

// Effects come out. The caller performs them.
while let Some(effect) = engine.next_effect() {
    match effect {
        Effect::Send(req)          => transport.enqueue(req),
        Effect::Persist(ops)       => storage.transact(&ops)?,
        Effect::ScheduleRetry(at)  => timer.set(at),
        Effect::Emit(telemetry)    => host.emit(telemetry),
    }
}
```

The engine decides *what* must happen. The caller decides *how*. In production the caller is the
FFI layer with real implementations; in the simulator it is seeded fakes. **The same core bytes
run in both worlds** — that is the entire trick.

## The four traits

```rust
trait Clock     { fn now(&self) -> Timestamp; }
trait Entropy   { fn fill(&mut self, buf: &mut [u8]); }   // UUIDv7, jitter
trait Storage   { fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome>; }
trait Transport { fn enqueue(&mut self, req: WireRequest) -> RequestId; }
```

They are **parameters of the core, never dependencies of it**. Time enters as `Event::Tick`,
randomness as bytes from `Entropy`, never as an ambient call.

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
cargo tree -p credsync-core        # no tokio, no reqwest, no rusqlite
cargo test -p credsync-core        # runs without a runtime
```

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
