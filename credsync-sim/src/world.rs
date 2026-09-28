//! The world: N devices and one server, driven forward through simulated time.
//!
//! Every device runs the **real** `credsync_core::Engine`. Nothing here reimplements the client;
//! the simulator's job is to be reality, badly behaved, and to watch what the engine does about
//! it.
//!
//! # Time compression
//!
//! The loop advances virtual time in steps of minutes, so a run of a few thousand steps covers
//! weeks of device life in a fraction of a second of CPU. Nothing sleeps, nothing waits on a real
//! clock, and no step costs more than the work inside it. That is the entire reason a thousand
//! seeds is a routine thing to run rather than an overnight job.

use crate::fakes::{
    Db, SimClock, SimCompressor, SimEntropy, SimStorage, SimTransport, StorageVerdict,
};
use crate::fault::{
    FLAP_DURATION_MS, Fault, FaultRates, SKEW_MAGNITUDE_MS, decide_response, decide_storage,
};
use crate::invariant::Invariants;
use crate::rng::Rng;
use crate::server::Server;
use crate::trace::Trace;
use credsync_core::{Engine, OutboxEntry, ScopeState, WireRequest, WireResponse};
use credsync_protocol::{
    Command, CommandId, CommandName, ConflictClass, Cursor, EntityName, EntityRegistration,
    HexString, Payload, PullRequest, SchemaVersion, ScopeCursor, ScopeDigest, ScopeId,
};
use std::cell::RefCell;
use std::rc::Rc;

/// How long one step of simulated time lasts.
const STEP_MS: i64 = 60_000;

/// Changes per pull batch, so a device walks forward rather than catching up in one go.
const PULL_LIMIT: usize = 4;

/// The engine, with every trait bound to its simulated counterpart.
type SimEngine = Engine<SimClock, SimEntropy, SimStorage, SimTransport, SimCompressor>;

/// A response in flight, waiting for its delivery time.
struct InFlight {
    device: usize,
    deliver_at_ms: i64,
    response: Result<WireResponse, credsync_core::TransportError>,
}

/// One simulated device.
struct Device {
    engine: SimEngine,
    clock: SimClock,
    storage: SimStorage,
    transport: SimTransport,
    /// True time at which connectivity returns, when flapped.
    offline_until_ms: i64,
    /// Requests this device has enqueued, for the periodic drop.
    requests_sent: u64,
    /// Commands this device has authored, so ids stay unique and deterministic.
    commands_authored: u8,
    /// Set when the next transaction commits and then loses its acknowledgement.
    ///
    /// The device must be **restarted** at that point, because that fault is a process kill: the
    /// write reached the disk and the engine died before hearing so. Letting the engine carry on
    /// would model something else entirely — a storage layer that lies about committing — and
    /// would leave in-memory state describing a database that has moved on without it.
    pending_kill: bool,
}

/// The whole simulation.
pub struct World {
    devices: Vec<Device>,
    server: Server,
    rng: Rng,
    rates: FaultRates,
    now_ms: i64,
    in_flight: Vec<InFlight>,
    /// Responses held back by a reorder fault, released after the next one.
    held: Vec<InFlight>,
    scope: ScopeId,
    entity: EntityName,
    /// While set, nobody authors new work: the world is draining, not living.
    ///
    /// Convergence is a claim about where things settle, and a world that keeps writing never
    /// settles. Turning the faults off is not enough — devices would carry on authoring and the
    /// server would carry on taking external writes, so the log would outrun the devices forever
    /// and every run would look divergent.
    quiet: bool,
    /// What the run recorded.
    pub trace: Trace,
    /// The claims being checked, and anything that has broken one.
    pub invariants: Invariants,
    /// A copy of the registry every device was built with, for policy conformance.
    registry: credsync_core::Registry,
    /// How hard the server is shedding load, and the levers it may pull.
    pub pressure: crate::pressure::Pressure,
}

impl World {
    /// Builds a world from a seed.
    ///
    /// Everything that varies between runs — device count, clock skew, drift, compression ratio —
    /// is drawn here from the seeded generator, so the seed alone determines the whole shape of
    /// the run and not merely its faults.
    #[must_use]
    pub fn new(seed: u64, rates: FaultRates, trace: Trace) -> Self {
        let mut rng = Rng::new(seed);
        let scope = ScopeId::new("inst:adl:enr:2026-cohort")
            .unwrap_or_else(|_| unreachable!("literal is a valid scope"));
        let entity = EntityName::new("reflections")
            .unwrap_or_else(|_| unreachable!("literal is a valid entity"));

        let device_count = rng.range(2, 4) as usize;
        let now_ms = 1_756_137_600_000;

        let mut server = Server::new();
        server.register_command("submit_reflection", entity.clone(), scope.clone());

        // The same declaration every device is built with, kept so the policy invariant is
        // generated from what the host registered rather than from a hardcoded list (D-047).
        let mut registry = credsync_core::Registry::new();
        register(&mut registry, &entity, &scope);

        let devices = (0..device_count)
            .map(|_| {
                // Each device gets its own skew and drift: docs/spec.md §6 is explicit that
                // device clocks lie, so none of them agree with the world or each other.
                let skew = rng.signed(SKEW_MAGNITUDE_MS);
                let drift = i64::from(rng.range(0, 400)) - 200;
                let clock = SimClock::new(now_ms, skew, drift);
                let storage = SimStorage::new();
                let transport = SimTransport::new();
                let entropy = SimEntropy::new(rng.next_u64());
                let compressor = SimCompressor::with_ratio(rng.range(1, 8) as usize);

                let mut engine = Engine::new(
                    clock.clone(),
                    entropy,
                    storage.clone(),
                    transport.clone(),
                    compressor,
                );
                register(engine.registry_mut(), &entity, &scope);

                Device {
                    engine,
                    clock,
                    storage,
                    transport,
                    offline_until_ms: 0,
                    requests_sent: 0,
                    commands_authored: 0,
                    pending_kill: false,
                }
            })
            .collect();

        Self {
            devices,
            server,
            rng,
            rates,
            now_ms,
            in_flight: Vec::new(),
            held: Vec::new(),
            scope,
            entity,
            quiet: false,
            trace,
            invariants: Invariants::new(),
            registry,
            pressure: crate::pressure::Pressure::new(),
        }
    }

    /// How many distinct commands the server has recorded an outcome for.
    #[must_use]
    pub fn server_answered_commands(&self) -> usize {
        self.server.answered_commands()
    }

    /// Whether the server recorded an outcome for this command id.
    #[must_use]
    pub fn server_has_answered(&self, id: &CommandId) -> bool {
        self.server.has_answered(id)
    }

    /// Every command any device ever enqueued, counted once.
    #[must_use]
    pub fn commands_ever_enqueued(&self) -> usize {
        let mut ids = std::collections::BTreeSet::new();
        for db in self.databases() {
            ids.extend(db.borrow().enqueued.iter().copied());
        }
        ids.len()
    }

    /// How many devices this run has.
    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Simulated milliseconds elapsed.
    #[must_use]
    pub const fn elapsed_ms(&self) -> i64 {
        self.now_ms - 1_756_137_600_000
    }

    /// Runs `steps` of simulated time, then lets the world go quiet and checks convergence.
    pub fn run(&mut self, steps: u32) {
        for _ in 0..steps {
            self.step();
        }
        self.settle();
    }

    /// Runs on with the faults switched off until every device has caught up.
    ///
    /// Convergence is the one claim that needs quiescence: a device mid-batch is *supposed* to
    /// disagree with the server, and calling that divergence would make the invariant fire on
    /// entirely normal operation — which is how an invariant gets switched off.
    ///
    /// So the network is made perfect and everyone is given time to finish. A device that still
    /// disagrees after that is genuinely divergent.
    pub fn settle(&mut self) {
        let hostile = self.rates;
        self.rates = FaultRates::none();
        self.quiet = true;
        for d in &self.devices {
            *d.transport.online.borrow_mut() = true;
        }
        // Enough cycles for the slowest device to walk the whole log at PULL_LIMIT changes per
        // pull, plus room for the latency of each round trip.
        for _ in 0..600 {
            self.step();
        }
        // Everything must have committed before convergence is judged. An uncommitted write is a
        // change no client could have been given, so counting it would fail every run.
        assert!(
            self.server.in_flight_writes() == 0,
            "settling left {} write(s) uncommitted",
            self.server.in_flight_writes()
        );
        self.rates = hostile;
        self.quiet = false;

        let dbs = self.databases();
        self.invariants
            .check_convergence(self.now_ms, &dbs, &self.server, &self.scope);
        self.invariants
            .check_policy(self.now_ms, &dbs, &self.registry);
    }

    /// One step: advance time, let each device act, carry the network, deliver what is due.
    fn step(&mut self) {
        self.now_ms += STEP_MS;

        for d in &self.devices {
            d.clock.advance_to(self.now_ms);
        }

        // Commits land before anything else in the step, so a write held open last step becomes
        // visible before this step's reads.
        self.server.advance_commits();

        // A writer holding its transaction open. The delay is drawn from the seed, so a run still
        // replays exactly — the simulator models concurrency without ever being concurrent.
        if !self.quiet && self.rng.chance(self.rates.slow_commit) {
            let delay = self.rng.range(1, 4);
            self.server.hold_next_write(delay);
            self.trace.fault(self.now_ms, 0, Fault::SlowCommit);
        }

        if self.rng.chance(self.rates.server_restart) {
            self.server.restart_cold();
            self.trace.fault(self.now_ms, 0, Fault::ServerRestarted);
        }

        // Load arrives and eases off on its own schedule, drawn from the seed. Ticked every step
        // whether or not a new episode starts, so an episode that began earlier ends.
        self.pressure.tick();
        if !self.quiet && !self.pressure.is_shedding() && self.rng.chance(self.rates.overload) {
            self.pressure.overload(&mut self.rng);
            self.trace.fault(self.now_ms, 0, Fault::Overloaded);
        }

        // Somebody else is always writing: another student, a teacher grading. Without this a
        // device would only ever see changes it caused.
        if !self.quiet && self.rng.chance(2) {
            let (scope, entity) = (self.scope.clone(), self.entity.clone());
            self.server.external_change(&scope, &entity, &mut self.rng);
        }

        for i in 0..self.devices.len() {
            self.device_step(i);
        }

        self.carry_network();
        self.deliver_due();

        // Continuously, not at quiescence. Design §7.1: a bug that self-corrects before the run
        // ends is still a bug — it corrupted state, and only a later event happening to paper
        // over it saved the user. Checking only at the end makes that whole class invisible.
        let dbs = self.databases();
        self.invariants.check_step(self.now_ms, &dbs, &self.scope);
        self.invariants
            .check_cursor_bounds(self.now_ms, &dbs, &self.server, &self.scope);
        self.invariants
            .check_durable_effects(self.now_ms, &dbs, &self.server, &self.scope);
        self.invariants
            .check_applied_state(self.now_ms, &dbs, &self.server, &self.scope);
    }

    /// One device's turn.
    fn device_step(&mut self, i: usize) {
        // Connectivity comes back when its flap expires.
        if self.devices[i].offline_until_ms > 0 && self.now_ms >= self.devices[i].offline_until_ms {
            self.devices[i].offline_until_ms = 0;
            *self.devices[i].transport.online.borrow_mut() = true;
        } else if self.devices[i].offline_until_ms == 0
            && self.rng.chance(self.rates.connectivity_flap)
        {
            self.devices[i].offline_until_ms = self.now_ms + FLAP_DURATION_MS;
            *self.devices[i].transport.online.borrow_mut() = false;
            self.trace.fault(self.now_ms, i, Fault::Flap);
        }

        if self.rng.chance(self.rates.device_restart) {
            self.restart_device(i);
        }

        // The user writes something. One percent per simulated minute is roughly a dozen
        // reflections a day, which is an active student rather than a load generator.
        if !self.quiet && self.rng.chance(1) {
            self.author_command(i);
        }

        // docs/spec.md §4: push precedes pull in every cycle, so a client immediately observes
        // the server's transformation of its own writes.
        self.push_step(i);
        self.pull_step(i);
    }

    /// Queues a command, with a storage fault decided first.
    fn author_command(&mut self, i: usize) {
        let n = self.devices[i].commands_authored;
        self.devices[i].commands_authored = n.wrapping_add(1);

        let id = command_id(i as u8, n);
        let command = Command {
            id,
            name: CommandName::new("submit_reflection")
                .unwrap_or_else(|_| unreachable!("literal is a valid name")),
            scope: self.scope.clone(),
            payload: Payload::new(serde_json::json!({ "body": "x".repeat(32) }))
                .unwrap_or_else(|_| unreachable!("literal is a valid payload")),
            // The device's own clock, which is skewed. docs/spec.md §6: advisory only, and
            // nothing may decide a conflict on it.
            client_ts: credsync_core::Clock::now(&self.devices[i].clock).as_millis(),
            checksum: hex_placeholder(),
        };

        self.arm_storage(i);
        let schema =
            SchemaVersion::new(1).unwrap_or_else(|_| unreachable!("1 is a valid schema version"));
        match self.devices[i]
            .engine
            .enqueue(OutboxEntry::new(command, schema))
        {
            Ok(()) => self.trace.event(self.now_ms, i, "enqueued"),
            Err(_) => self.trace.event(self.now_ms, i, "enqueue-refused"),
        }
        self.maybe_kill(i);
    }

    /// Carries out a kill armed by [`arm_storage`](Self::arm_storage).
    ///
    /// Called immediately after every engine call that could have transacted. The process died
    /// with the write on disk, so recovery is a restart that reloads from storage and replays the
    /// outbox against a server that dedupes — not the engine reasoning about what happened.
    fn maybe_kill(&mut self, i: usize) {
        if self.devices[i].pending_kill {
            self.devices[i].pending_kill = false;
            self.restart_device(i);
        }
    }

    /// Decides whether this device's next storage transaction misbehaves.
    fn arm_storage(&mut self, i: usize) {
        let verdict = match decide_storage(&mut self.rng, &self.rates) {
            Some(Fault::StorageFailed) => {
                self.trace.fault(self.now_ms, i, Fault::StorageFailed);
                StorageVerdict::FailBeforeCommit
            }
            Some(Fault::StorageCommittedThenKilled) => {
                self.trace
                    .fault(self.now_ms, i, Fault::StorageCommittedThenKilled);
                self.devices[i].pending_kill = true;
                StorageVerdict::CommitThenLoseAck
            }
            // The same lie, without the restart. The restart is what made the paired version
            // survivable -- a restarted engine reloads from storage and never consults the state
            // the lie invalidated -- so this is the one that reaches the defence (#55).
            Some(Fault::StorageLiedAboutCommit) => {
                self.trace
                    .fault(self.now_ms, i, Fault::StorageLiedAboutCommit);
                StorageVerdict::CommitThenLoseAck
            }
            _ => StorageVerdict::Commit,
        };
        self.devices[i].storage.0.borrow_mut().verdict = verdict;
    }

    /// Builds and sends a push, if there is anything queued.
    fn push_step(&mut self, i: usize) {
        let Ok(Some(request)) = self.devices[i]
            .engine
            .build_push(Server::protocol(), 100_000)
        else {
            return;
        };
        if request.commands.is_empty() {
            return;
        }
        let mut transport = self.devices[i].transport.clone();
        let _ = credsync_core::Transport::enqueue(&mut transport, WireRequest::Push(request));
    }

    /// Sends a pull for this device's scope.
    fn pull_step(&mut self, i: usize) {
        // A scope whose cached cursor is of unknown accuracy must not be pulled from: the cursor
        // may point past rows a rebuild cleared, or behind writes that committed and were reported
        // as failed. The next apply reloads it from storage and clears the flag; until then, a
        // request built from it would ask the server to resume from a position this device cannot
        // support.
        if self.devices[i].engine.needs_reload(&self.scope) {
            let scope = self.scope.clone();
            if self.devices[i].engine.reload_scope(&scope).is_err() {
                // Storage is still unreadable. Nothing to pull from, and nothing lost by waiting:
                // the scope stays suspect and the next cycle tries again.
                return;
            }
        }

        let cursor = self.devices[i]
            .engine
            .scope_state(&self.scope)
            .map_or(Cursor::START, |s| s.cursor);

        let request = PullRequest {
            protocol: Server::protocol(),
            scopes: vec![ScopeCursor {
                scope: self.scope.clone(),
                cursor,
            }],
            limit_bytes: None,
        };
        let mut transport = self.devices[i].transport.clone();
        let _ = credsync_core::Transport::enqueue(&mut transport, WireRequest::Pull(request));
    }

    /// Takes everything the devices enqueued, answers it, and applies faults.
    fn carry_network(&mut self) {
        for i in 0..self.devices.len() {
            for out in self.devices[i].transport.drain() {
                self.devices[i].requests_sent += 1;
                let index = self.devices[i].requests_sent;

                let fault = decide_response(&mut self.rng, &self.rates, index);
                if let Some(f) = fault {
                    self.trace.fault(self.now_ms, i, f);
                }
                if matches!(fault, Some(Fault::Dropped)) {
                    continue;
                }

                // A server that decodes cleanly and is still wrong. Distinct from `malformed`,
                // which produces bytes that do not decode at all — this is a buggy or hostile
                // peer, and the client's ordering and dedupe checks exist for precisely it.
                //
                // Added at CS-13: the drill planted an ordering bug and a dedupe bug that the
                // unit tests caught instantly and the simulator could not reach, because a server
                // built from its own log never repeats a seq and a push built from the outbox
                // never names a command twice.
                let violate = self.rng.chance(self.rates.protocol_violation);

                let response = match &out.request {
                    WireRequest::Pull(req) => {
                        let budget = self.pressure.budget_bytes();
                        let mut pulled = self.server.pull(req, PULL_LIMIT, budget);
                        if violate && repeat_last_change(&mut pulled) {
                            self.trace.fault(self.now_ms, i, Fault::ProtocolViolation);
                        }
                        Ok(WireResponse::Pull(pulled))
                    }
                    WireRequest::Push(req) => {
                        // Backpressure: a loaded server looks at a prefix and answers only those. The
                        // rest stay in the outbox, because the client resolves only ids it is told
                        // about. It never answers a command it did not process -- see `pressure`.
                        let accepted = self.pressure.accept_count(req.commands.len());
                        let mut pushed = self.server.push_prefix(req, accepted, &mut self.rng);
                        if violate && repeat_last_result(&mut pushed) {
                            self.trace.fault(self.now_ms, i, Fault::ProtocolViolation);
                        }
                        Ok(WireResponse::Push(pushed))
                    }
                    WireRequest::Bootstrap(_) => continue,
                    _ => continue,
                };

                // Severed and malformed both arrive as a transport-level failure, because that is
                // what the client sees: bytes that did not decode. The distinction matters to the
                // trace, not to the engine.
                let response = match fault {
                    Some(Fault::Severed) => Err(credsync_core::TransportError::Malformed {
                        detail: "severed mid-batch".to_owned(),
                    }),
                    Some(Fault::Malformed) => Err(credsync_core::TransportError::Malformed {
                        detail: "corrupted bytes".to_owned(),
                    }),
                    _ => response,
                };

                let latency = i64::from(
                    self.rng
                        .range(self.rates.latency_ms.0, self.rates.latency_ms.1),
                );
                let flight = InFlight {
                    device: i,
                    deliver_at_ms: self.now_ms + latency,
                    response,
                };

                match fault {
                    Some(Fault::Duplicated) => {
                        // The same answer twice, at different times.
                        self.in_flight.push(InFlight {
                            device: i,
                            deliver_at_ms: flight.deliver_at_ms + i64::from(self.rng.range(1, 500)),
                            response: clone_response(&flight.response),
                        });
                        self.in_flight.push(flight);
                    }
                    Some(Fault::Reordered) => self.held.push(flight),
                    _ => self.in_flight.push(flight),
                }
            }
        }

        // Held responses rejoin the queue behind whatever overtook them.
        for mut h in std::mem::take(&mut self.held) {
            h.deliver_at_ms = self.now_ms + STEP_MS + i64::from(self.rng.range(1, 900));
            self.in_flight.push(h);
        }
    }

    /// Hands every response whose time has come to the device that asked for it.
    fn deliver_due(&mut self) {
        // Partitioned rather than sorted: delivery order among simultaneous responses is the
        // order they were queued, which is deterministic, and sorting would need a tiebreak that
        // is one more thing to get wrong.
        let due: Vec<InFlight> = {
            let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.in_flight)
                .into_iter()
                .partition(|f| f.deliver_at_ms <= self.now_ms);
            self.in_flight = waiting;
            ready
        };

        for f in due {
            let i = f.device;
            match f.response {
                Err(_) => self.trace.event(self.now_ms, i, "response-failed"),
                Ok(WireResponse::Pull(response)) => {
                    for batch in &response.batches {
                        self.arm_storage(i);
                        match self.devices[i].engine.apply_batch(batch) {
                            Ok(applied) => {
                                self.trace.event(
                                    self.now_ms,
                                    i,
                                    if applied.diverged {
                                        "applied-diverged"
                                    } else {
                                        "applied"
                                    },
                                );
                            }
                            Err(_) => self.trace.event(self.now_ms, i, "apply-refused"),
                        }
                        self.maybe_kill(i);
                    }
                }
                Ok(WireResponse::Push(response)) => {
                    self.arm_storage(i);
                    match self.devices[i].engine.apply_results(&response) {
                        Ok(r) => {
                            let _ = r;
                            self.trace.event(self.now_ms, i, "results-applied");
                        }
                        Err(_) => self.trace.event(self.now_ms, i, "results-refused"),
                    }
                    self.maybe_kill(i);
                }
                Ok(_) => self.trace.event(self.now_ms, i, "response-ignored"),
            }
        }
    }

    /// Restarts a device: a fresh engine, reloading everything from storage.
    ///
    /// This is what makes the kill-between-commit-and-ack fault mean anything. The engine's
    /// in-memory state is discarded; whatever the database holds is the truth, and the outbox is
    /// replayed against a server that dedupes.
    fn restart_device(&mut self, i: usize) {
        self.trace.fault(self.now_ms, i, Fault::DeviceRestarted);

        let storage = self.devices[i].storage.clone();
        let clock = self.devices[i].clock.clone();
        let transport = SimTransport::new();
        let entropy = SimEntropy::new(self.rng.next_u64());
        let compressor = SimCompressor::default();

        let mut engine = Engine::new(
            clock.clone(),
            entropy,
            storage.clone(),
            transport.clone(),
            compressor,
        );
        register(engine.registry_mut(), &self.entity, &self.scope);

        // Restore from what the database holds, which is the whole point of the restart.
        let db = storage.0.borrow();
        if let Some(cursor) = db.cursors.get(&self.scope) {
            let digest = db
                .digests
                .get(&self.scope)
                .and_then(digest_from_hex)
                .unwrap_or(ScopeDigest::EMPTY);
            engine.restore_scope(self.scope.clone(), ScopeState::restored(*cursor, digest));
        }
        engine.restore_outbox(
            db.outbox
                .iter()
                .map(|(c, s)| OutboxEntry::new(c.clone(), *s)),
        );
        drop(db);

        self.devices[i].engine = engine;
        self.devices[i].transport = transport;
        self.devices[i].clock = clock;
        self.devices[i].pending_kill = false;
    }

    /// Every device's database, for the invariants at CS-12 to read.
    #[must_use]
    pub fn databases(&self) -> Vec<Rc<RefCell<Db>>> {
        self.devices.iter().map(|d| d.storage.0.clone()).collect()
    }

    /// The server, for the invariants at CS-12 to compare against.
    #[must_use]
    pub const fn server(&self) -> &Server {
        &self.server
    }

    /// What each device currently believes the time is, and what it actually is.
    ///
    /// Exposed so a test can check the clocks really do disagree. `docs/spec.md` §6 rests on
    /// device clocks lying, and a simulator whose clocks all told the truth would let a
    /// `client_ts`-dependent bug pass every run it ever did.
    #[must_use]
    pub fn apparent_times_ms(&self) -> (i64, Vec<i64>) {
        (
            self.now_ms,
            self.devices
                .iter()
                .map(|d| credsync_core::Clock::now(&d.clock).as_millis())
                .collect(),
        )
    }

    /// Turns off the server's commit-order guard, so it hands over changes whose predecessors
    /// have not committed.
    ///
    /// For the drill in `tests/commit_order.rs` and nothing else. A run with this set models the
    /// bug D-063 fixed, and the invariants must catch it — a simulator that cannot fail on a bug
    /// class is a simulator that does not test it.
    pub const fn disable_commit_order_guard(&mut self) {
        self.server.commit_order_guard = false;
    }

    /// The scope every device in this run subscribes to.
    #[must_use]
    pub const fn scope(&self) -> &ScopeId {
        &self.scope
    }
}

/// Declares the entity and command every device in a run uses.
fn register(registry: &mut credsync_core::Registry, entity: &EntityName, scope: &ScopeId) {
    registry.register_entity(EntityRegistration {
        entity: entity.clone(),
        scope: scope.clone(),
        conflict_class: ConflictClass::OwnerDraft,
        schema_version: SchemaVersion::new(1)
            .unwrap_or_else(|_| unreachable!("1 is a valid schema version")),
    });
    registry.register_command(
        CommandName::new("submit_reflection")
            .unwrap_or_else(|_| unreachable!("literal is a valid name")),
        entity.clone(),
    );
}

/// A UUIDv7 built from a device index and a counter, so ids are unique and reproducible.
fn command_id(device: u8, n: u8) -> CommandId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[1] = 0x91;
    bytes[6] = 0x70;
    bytes[14] = device;
    bytes[15] = n;
    CommandId::from_bytes(bytes).unwrap_or_else(|_| unreachable!("version nibble is 7"))
}

/// Repeats the last result in a push response, so one command is answered twice.
///
/// `docs/spec.md` §3.3 gives one result per submitted command. A client that resolves both copies
/// records two outcomes for one command, and if the verdicts differ the stored one depends on
/// write order — a dead letter for a command the host applied, or an "applied" masking a
/// rejection the user needed to see.
///
/// Returns whether anything was repeated, so the trace never counts a fault that did not happen.
fn repeat_last_result(response: &mut credsync_protocol::PushResponse) -> bool {
    if let Some(last) = response.results.last().cloned() {
        response.results.push(last);
        return true;
    }
    false
}

/// Breaks a protocol rule in a batch that is otherwise perfectly well-formed.
///
/// Repeats the last change, `seq` and all. `docs/spec.md` §4 requires changes within a scope to be
/// strictly `seq`-ordered, so a client that accepts this applies one change twice — and since the
/// scope digest deliberately does not cancel duplicates (D-032), the damage persists and surfaces
/// much later as divergence pointing nowhere near its cause.
///
/// Returns whether anything was actually corrupted, so the trace does not record a fault that did
/// not happen: an empty batch has no change to repeat, and a fault counted but never applied is a
/// coverage report that lies.
fn repeat_last_change(response: &mut credsync_protocol::PullResponse) -> bool {
    for batch in &mut response.batches {
        if let Some(last) = batch.changes.last().cloned() {
            batch.changes.push(last);
            return true;
        }
    }
    false
}

fn hex_placeholder() -> HexString {
    HexString::new("00000000000000000000000000000000")
        .unwrap_or_else(|_| unreachable!("32 zeros is valid hex"))
}

/// Rebuilds a digest from its wire form, for restoring a scope after a restart.
fn digest_from_hex(hex: &HexString) -> Option<ScopeDigest> {
    u128::from_str_radix(hex.as_str(), 16)
        .ok()
        .map(ScopeDigest::from_raw)
}

/// Responses are cloned for the duplicate fault; errors do not implement `Clone` uniformly, so
/// this rebuilds the shape rather than deriving it.
fn clone_response(
    r: &Result<WireResponse, credsync_core::TransportError>,
) -> Result<WireResponse, credsync_core::TransportError> {
    match r {
        Ok(v) => Ok(v.clone()),
        Err(e) => Err(e.clone()),
    }
}
