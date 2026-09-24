//! Schema migrations. `docs/spec.md` §7, Design v2.1 §5.
//!
//! # A queued command is never dropped
//!
//! The rule the whole module serves. A device offline for three weeks may hold commands authored
//! under a schema the server has since moved past. `docs/spec.md` §7 is explicit: *"A command whose
//! schema the server no longer accepts is queued, never dropped."*
//!
//! Dropping would be the easy implementation and it is the one that loses a student's coursework
//! while reporting success. So there is no path here that discards anything: a migration either
//! produces a newer document, or the original is **quarantined intact** and surfaced.
//!
//! # Migrations are pure functions, registered by the host
//!
//! credSync does not know what a reflection is, so it cannot know how one becomes a v2 reflection.
//! The host registers single-step up-migrations and this module chains them.
//!
//! Single steps, not arbitrary jumps. A host that could register `v1 -> v3` directly *and* a
//! `v1 -> v2 -> v3` chain would have two answers to one question, and the spec requires they agree:
//! *"Migration composition is associative: v1→v2→v3 equals v1→v3."* Registering only steps makes
//! that structural rather than hoped for — there is one path, so the two cannot disagree.
//!
//! # Forward only
//!
//! A down-migration is refused rather than attempted. An older app reading a newer row cannot
//! usually reconstruct what it does not understand, and a "best effort" downgrade silently discards
//! the fields it has no home for — which is data loss wearing a helpful expression. The honest
//! answer is to refuse and tell the user to update.

use credsync_protocol::{EntityName, SchemaVersion};
use serde_json::Value;
use std::collections::BTreeMap;

/// A single-step up-migration: one schema version to the next.
///
/// Takes the document's JSON and returns the migrated JSON, or a reason it could not. A function
/// pointer rather than a boxed closure so the registry stays `Clone` and `Debug`, which the engine
/// and the simulator both rely on.
pub type MigrationFn = fn(&Value) -> Result<Value, String>;

/// Why a migration could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MigrationError {
    /// No registered chain reaches the target version.
    ///
    /// Carries the step that is missing, because "no path from 1 to 4" sends an operator hunting
    /// through four registrations while "no step from 2 to 3" names the one that is absent.
    NoPath {
        /// The entity being migrated.
        entity: EntityName,
        /// The version the chain got stuck at.
        stuck_at: u16,
        /// Where it was trying to reach.
        target: u16,
    },

    /// The host's migration returned an error.
    Failed {
        /// The entity being migrated.
        entity: EntityName,
        /// Which step failed.
        from: u16,
        /// What it was migrating to.
        to: u16,
        /// The host's explanation.
        reason: String,
    },

    /// The migrated document no longer satisfies the protocol's limits.
    ///
    /// A migration that grows a snapshot past 256 KB has produced something unsendable. Refused
    /// here rather than at encode time, so the original is quarantined intact instead of a
    /// half-migrated value reaching storage.
    Invalid {
        /// The entity being migrated.
        entity: EntityName,
        /// What was wrong.
        detail: String,
    },

    /// A backward migration was requested.
    ///
    /// Never attempted. See the module docs: a best-effort downgrade discards the fields it has no
    /// home for, which is data loss that reports success.
    Backward {
        /// Where it started.
        from: u16,
        /// Where it was asked to go.
        to: u16,
    },
}

impl core::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoPath {
                entity,
                stuck_at,
                target,
            } => write!(
                f,
                "no registered migration for {entity} from schema {stuck_at}, needed to reach {target}"
            ),
            Self::Failed {
                entity,
                from,
                to,
                reason,
            } => write!(f, "migrating {entity} from {from} to {to} failed: {reason}"),
            Self::Invalid { entity, detail } => {
                write!(f, "the migrated {entity} document is invalid: {detail}")
            }
            Self::Backward { from, to } => {
                write!(f, "refused to migrate backward, from {from} to {to}")
            }
        }
    }
}

impl core::error::Error for MigrationError {}

/// The host's registered up-migrations.
///
/// Keyed by `(entity, from_version)`. Each entry advances exactly one version, and [`plan`] chains
/// them.
///
/// [`plan`]: Migrations::plan
#[derive(Debug, Clone, Default)]
pub struct Migrations {
    /// `BTreeMap` rather than `HashMap`: iteration order is load-bearing in a crate whose value is
    /// deterministic replay, and a migration registry that enumerated in a different order between
    /// runs would break the simulator's byte-identical traces.
    steps: BTreeMap<(EntityName, u16), MigrationFn>,
}

impl Migrations {
    /// An empty registry. An entity with no migrations can still be read at its current version.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            steps: BTreeMap::new(),
        }
    }

    /// Registers a single-step up-migration from `from` to `from + 1`.
    ///
    /// Only the source version is named, because the destination is always the next one. A
    /// signature that let the host write `register(entity, 1, 3, f)` would invite exactly the
    /// two-paths-one-question problem the module docs describe.
    pub fn register(&mut self, entity: EntityName, from: SchemaVersion, f: MigrationFn) {
        self.steps.insert((entity, from.get()), f);
    }

    /// Whether any migration is registered for an entity.
    #[must_use]
    pub fn has_any(&self, entity: &EntityName) -> bool {
        self.steps
            .keys()
            .any(|(registered, _)| registered == entity)
    }

    /// Checks a chain exists from `from` to `to` without running it.
    ///
    /// # Errors
    /// Returns [`MigrationError::NoPath`] naming the first missing step, or
    /// [`MigrationError::Backward`] if `to < from`.
    pub fn plan(
        &self,
        entity: &EntityName,
        from: SchemaVersion,
        to: SchemaVersion,
    ) -> Result<(), MigrationError> {
        let (from, to) = (from.get(), to.get());
        if to < from {
            return Err(MigrationError::Backward { from, to });
        }
        for version in from..to {
            if !self.steps.contains_key(&(entity.clone(), version)) {
                return Err(MigrationError::NoPath {
                    entity: entity.clone(),
                    stuck_at: version,
                    target: to,
                });
            }
        }
        Ok(())
    }

    /// Migrates raw JSON from one schema version to another, one step at a time.
    ///
    /// `to == from` returns the input untouched, which is the common case and must stay cheap: it
    /// runs for every row of every batch.
    ///
    /// # Errors
    /// Returns [`MigrationError`] if no chain exists, a step fails, or a backward migration was
    /// requested. **The input is never partially migrated on the way out** — a failure mid-chain
    /// discards the intermediate value and reports, so the caller still holds the original to
    /// quarantine.
    pub fn migrate_value(
        &self,
        entity: &EntityName,
        value: &Value,
        from: SchemaVersion,
        to: SchemaVersion,
    ) -> Result<Value, MigrationError> {
        let (from_n, to_n) = (from.get(), to.get());
        if to_n < from_n {
            return Err(MigrationError::Backward {
                from: from_n,
                to: to_n,
            });
        }
        if to_n == from_n {
            return Ok(value.clone());
        }

        let mut current = value.clone();
        for version in from_n..to_n {
            let step = self.steps.get(&(entity.clone(), version)).ok_or_else(|| {
                MigrationError::NoPath {
                    entity: entity.clone(),
                    stuck_at: version,
                    target: to_n,
                }
            })?;
            current = step(&current).map_err(|reason| MigrationError::Failed {
                entity: entity.clone(),
                from: version,
                to: version + 1,
                reason,
            })?;
        }
        Ok(current)
    }
}
