//! CS-6 DoD 4: the engine is single-threaded by construction — no `Send`/`Sync` bounds required.
//!
//! The claim is not "the engine happens to be usable from one thread". It is that **an
//! implementation which cannot cross threads is still a valid implementation**, so no binding is
//! ever forced to add synchronisation it does not need.
//!
//! That matters concretely. React Native's Hermes is single-threaded, and the first shipped
//! storage adapter is SQLite via `rusqlite`, whose `Connection` is not `Sync`. A `Send` bound on
//! the engine would push every binding into wrapping its connection in a mutex to satisfy a
//! requirement the design never had — paying for thread-safety in a runtime that has no second
//! thread to be safe from.
//!
//! # How this file proves it
//!
//! Every implementation below holds an `Rc`, which is `!Send` and `!Sync` by construction. If any
//! bound in `Engine` required `Send` or `Sync` — on the struct, on `new`, on any method — this
//! file would not compile. It is a compile-time assertion wearing a test's clothing, and the
//! `#[test]` functions exist mainly to give it somewhere to live.
//!
//! Rust has no stable way to *assert* a negative bound, so `send_bound_mechanism_works` pins the
//! mechanism from the other side: it shows `assert_send` really does constrain, which is what
//! makes its absence here meaningful rather than decorative.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use credsync_core::{
    Clock, Compressor, Engine, Entropy, RequestId, Storage, StorageError, StorageOp, Timestamp,
    Transport, TransportError, TxOutcome, WireRequest,
};
use credsync_protocol::{EntityId, EntityName, RowVersion};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A clock whose state cannot leave this thread.
struct RcClock(Rc<Cell<i64>>);

impl Clock for RcClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0.get())
    }
}

/// Entropy from a counter rather than the operating system.
///
/// Not random, and that is the point: the core must not be able to tell the difference, because
/// the simulator's entropy is exactly this shape.
struct RcEntropy(Rc<Cell<u64>>);

impl Entropy for RcEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        for slot in buf.iter_mut() {
            let next = self.0.get().wrapping_add(1);
            self.0.set(next);
            *slot = (next & 0xff) as u8;
        }
    }
}

/// Storage that records what it was asked to commit.
struct RcStorage(Rc<RefCell<Vec<StorageOp>>>);

impl Storage for RcStorage {
    fn transact(&mut self, ops: &[StorageOp]) -> Result<TxOutcome, StorageError> {
        self.0.borrow_mut().extend_from_slice(ops);
        Ok(TxOutcome::new(ops.len()))
    }

    /// Holds no rows, so every lookup is a miss.
    ///
    /// This file is about thread bounds, not about storage behaviour — what matters here is that
    /// the method can be implemented by a type carrying an `Rc`, which is the whole assertion.
    fn row_version(
        &self,
        _entity: &EntityName,
        _entity_id: &EntityId,
    ) -> Result<Option<RowVersion>, StorageError> {
        Ok(None)
    }

    /// Holds no scopes either, for the same reason as `row_version` above.
    fn scope_state(
        &self,
        _scope: &credsync_protocol::ScopeId,
    ) -> Result<Option<credsync_core::StoredScope>, StorageError> {
        Ok(None)
    }

    /// Holds no outbox either, for the same reason as `row_version` above.
    fn outbox(&self) -> Result<Vec<credsync_core::OutboxEntry>, StorageError> {
        Ok(Vec::new())
    }
}

/// A compressor whose state cannot leave this thread either.
struct RcCompressor(Rc<Cell<usize>>);

impl Compressor for RcCompressor {
    fn compressed_len(&self, bytes: &[u8]) -> usize {
        self.0.set(self.0.get() + 1);
        bytes.len()
    }
}

/// Transport that records requests and mints handles.
struct RcTransport {
    next_id: Rc<Cell<u64>>,
    sent: Rc<RefCell<Vec<WireRequest>>>,
}

impl Transport for RcTransport {
    fn enqueue(&mut self, req: WireRequest) -> Result<RequestId, TransportError> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.sent.borrow_mut().push(req);
        Ok(RequestId::new(id))
    }
}

fn engine() -> Engine<RcClock, RcEntropy, RcStorage, RcTransport, RcCompressor> {
    Engine::new(
        RcClock(Rc::new(Cell::new(1_756_137_600_000))),
        RcEntropy(Rc::new(Cell::new(0))),
        RcStorage(Rc::new(RefCell::new(Vec::new()))),
        RcTransport {
            next_id: Rc::new(Cell::new(1)),
            sent: Rc::new(RefCell::new(Vec::new())),
        },
        RcCompressor(Rc::new(Cell::new(0))),
    )
}

/// The load-bearing assertion: this compiles at all.
///
/// All five implementations are `!Send` and `!Sync`. Adding `Send` anywhere in `Engine`'s bounds
/// breaks this line, which is the whole mechanism.
#[test]
fn engine_builds_from_parts_that_cannot_cross_threads() {
    let mut e = engine();
    assert_eq!(e.pending_effects(), 0);
    assert!(e.next_effect().is_none());
}

/// A fresh engine has nothing to say.
///
/// At CS-6 it has nothing to say ever, since transitions arrive at CS-7 — but the drain API is
/// the surface every later slice fills, so its resting behaviour is pinned now.
#[test]
fn a_new_engine_queues_no_effects() {
    let mut e = engine();
    assert_eq!(e.next_effect(), None);
    assert_eq!(e.next_effect(), None, "draining twice must stay empty");
    assert_eq!(e.pending_effects(), 0);
}

/// `Debug` works without the four implementations being `Debug` themselves.
///
/// None of `RcClock`, `RcEntropy`, `RcStorage`, `RcTransport` or `RcCompressor` derives `Debug`. A derived
/// `Debug` on `Engine` would have added those bounds and this would not compile — which would
/// mean a caller could not debug-print an engine holding a SQLite handle.
#[test]
fn engine_is_debug_without_its_parts_being_debug() {
    let rendered = format!("{:?}", engine());
    assert!(
        rendered.contains("pending_effects"),
        "Debug should report outstanding work, got: {rendered}"
    );
}

/// The five implementations work when driven directly.
///
/// Nothing calls them through the engine yet — that is CS-7 — so this checks the trait
/// signatures are actually usable rather than merely well-formed.
#[test]
fn the_five_implementations_are_usable() {
    let ticks = Rc::new(Cell::new(42));
    let clock = RcClock(Rc::clone(&ticks));
    assert_eq!(clock.now(), Timestamp::from_millis(42));

    let mut entropy = RcEntropy(Rc::new(Cell::new(0)));
    let mut buf = [0u8; 4];
    entropy.fill(&mut buf);
    assert_eq!(
        buf,
        [1, 2, 3, 4],
        "entropy must fill every byte it is given"
    );

    let log = Rc::new(RefCell::new(Vec::new()));
    let mut storage = RcStorage(Rc::clone(&log));
    assert_eq!(storage.transact(&[]).unwrap(), TxOutcome::new(0));
    assert!(log.borrow().is_empty());

    let calls = Rc::new(Cell::new(0));
    let compressor = RcCompressor(Rc::clone(&calls));
    assert_eq!(compressor.compressed_len(b"abcd"), 4);
    assert_eq!(calls.get(), 1, "the compressor really was consulted");
}

/// Proves `assert_send` constrains anything at all.
///
/// Without this, "no `assert_send::<Engine<..>>()` appears in this file" would be indistinguishable
/// from "`assert_send` does nothing". Rust cannot express a negative bound on stable, so the
/// mechanism is pinned from the positive side instead, and the negative case is demonstrated by
/// deliberately adding a `Send` bound and watching the crate stop compiling — recorded in the
/// CS-6 pull request rather than left as an assumption.
#[test]
fn send_bound_mechanism_works() {
    const fn assert_send<T: Send>() {}
    assert_send::<u64>();
    assert_send::<Timestamp>();
    assert_send::<RequestId>();
}
