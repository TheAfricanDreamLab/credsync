//! What the engine asks the caller to do.
//!
//! Storage and transport are not here. The engine holds those traits and performs them itself
//! (D-037), so an effect is specifically *something the four traits cannot express*: arranging to
//! be woken later, and telling the host something worth knowing.
//!
//! Effects are queued, not executed. The engine appends; the caller drains with
//! [`Engine::next_effect`](crate::Engine::next_effect). Queueing rather than calling back keeps
//! the engine free of any borrow of the caller, and keeps the effect stream a *value* — which is
//! what lets the simulator assert that two runs of the same seed produced byte-identical
//! behaviour.

use crate::types::Timestamp;
use credsync_protocol::{EntityId, EntityName, HexString, SchemaVersion, ScopeId};

/// Something the caller must do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effect {
    /// Wake the engine with [`Event::Tick`](crate::Event::Tick) at or after this time.
    ///
    /// The engine never sleeps. `docs/spec.md` §2: backoff is exponential with jitter, drawn from
    /// the client's seeded entropy so simulated runs replay identically — which means the
    /// *deadline* is computed here and the *waiting* happens outside, on a platform timer that
    /// survives the app being backgrounded.
    ///
    /// A caller that fires this late is fine. One that fires it early is also fine: the engine
    /// re-checks the deadline against the `now` it is handed rather than trusting the wake-up.
    ScheduleRetry {
        /// When the engine wants to be woken.
        at: Timestamp,
    },

    /// Something the host should record.
    Emit(Telemetry),
}

/// Observable facts worth reporting to the host.
///
/// Not logging. Each variant is something an operator needs to see in aggregate, and the set stays
/// small on purpose — telemetry that reports everything is telemetry nobody reads. The full
/// surface lands at CS-31 (#32); what is here is what `docs/spec.md` already requires.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Telemetry {
    /// A row arrived under a schema this app could not migrate, and was set aside.
    ///
    /// Worth surfacing rather than logging: it means a device is holding data it cannot display,
    /// which the user will experience as something missing. In aggregate it is also the signal
    /// that a host shipped a schema bump without the matching migration — one device reporting it
    /// is a curiosity, a cohort reporting it is an incident.
    RowQuarantined {
        /// Which entity the row belongs to.
        entity: EntityName,
        /// The row's identifier.
        entity_id: EntityId,
        /// The schema the row is written under.
        schema_version: SchemaVersion,
        /// Why it could not be migrated.
        reason: String,
    },

    /// A queued command could not be migrated forward, so it was held rather than sent.
    ///
    /// **Held, not dropped.** `docs/spec.md` §7: *"A command whose schema the server no longer
    /// accepts is queued, never dropped."* The entry stays in the outbox and this reports why it
    /// is not moving, so the user is told rather than left watching an edit that silently never
    /// saves.
    CommandHeld {
        /// Which command is stuck.
        command: credsync_protocol::CommandId,
        /// The schema it was authored under.
        authored_under: SchemaVersion,
        /// Why it could not be migrated.
        reason: String,
    },

    /// A scope's digest did not match the server's after applying a batch.
    ///
    /// **Silent divergence** — the class of bug that testing missed and users never report until
    /// trust is gone. `docs/spec.md` §5 requires that the report carry *both* digests, because
    /// the difference between them is the only evidence of what went wrong, and by the time
    /// anyone looks the client will have re-bootstrapped and destroyed the divergent state.
    ///
    /// The engine's response is separate from reporting it: mark the scope tainted, re-bootstrap,
    /// replay the outbox. That lands at CS-22 (#23). A tainted scope does not block others.
    ScopeDiverged {
        /// The scope that diverged.
        scope: ScopeId,
        /// What this client computed.
        client: HexString,
        /// What the server reported.
        server: HexString,
    },
}
