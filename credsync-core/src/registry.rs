//! The entity registry: which entities exist, what class each is, and what a command may target.
//!
//! Design v2.1 §6: *"the registry declares each entity's class... refused by the registry, not by
//! convention."* That last clause is the whole point of this module. "Do not send commands for
//! grades" is a sentence in a document; a registry that refuses them is a mechanism.
//!
//! # Why commands must declare their target entity
//!
//! A [`Command`] carries a *name* — `submit_reflection` — not an entity. The name is a domain
//! operation, and only the host knows which table it writes. So the registry holds that mapping
//! too: registering a command says which entity it targets, and the entity's registration says
//! whether commands may target it at all.
//!
//! Without the mapping, "no command may target a server-authoritative entity" would be
//! unenforceable — the engine would be looking at an opaque name and a payload it does not model.

use credsync_protocol::{Command, CommandName, ConflictClass, EntityName, EntityRegistration};
use std::collections::BTreeMap;

/// Why the registry refused something.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistryError {
    /// The command name has not been registered, so its target is unknown.
    ///
    /// Refused rather than allowed. An unregistered command is one whose class nobody declared,
    /// and defaulting to "permitted" would mean the protection in this module applies only to
    /// entities someone remembered to register — which is the convention it exists to replace.
    UnknownCommand {
        /// The unregistered name.
        name: CommandName,
    },

    /// The command's target entity has not been registered.
    UnknownEntity {
        /// The unregistered entity.
        entity: EntityName,
    },

    /// A command targeted a server-authoritative entity.
    ///
    /// `docs/spec.md` §6: institution truth — grades, scores, schedules, statuses — is **pull
    /// only**. A client that could write them could change its own grade, and the refusal must
    /// therefore happen before the command reaches the outbox rather than at the server, where a
    /// modified client simply would not ask.
    ServerAuthoritative {
        /// The entity that refuses commands.
        entity: EntityName,
    },
}

impl core::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownCommand { name } => {
                write!(
                    f,
                    "command '{name}' is not registered, so its target is unknown"
                )
            }
            Self::UnknownEntity { entity } => write!(f, "entity '{entity}' is not registered"),
            Self::ServerAuthoritative { entity } => write!(
                f,
                "entity '{entity}' is server-authoritative and accepts no commands"
            ),
        }
    }
}

impl core::error::Error for RegistryError {}

/// What the host has declared about its entities and commands.
///
/// Empty by default. An engine with an empty registry accepts no commands at all, which is the
/// safe direction to fail: a host that forgot to register gets a loud refusal on its first write
/// rather than a silent loss of the protection.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// `BTreeMap`, not `HashMap` — see `Engine`'s `scopes` field for why iteration order is
    /// load-bearing in a crate whose whole value is deterministic replay.
    entities: BTreeMap<EntityName, EntityRegistration>,
    commands: BTreeMap<CommandName, EntityName>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entities: BTreeMap::new(),
            commands: BTreeMap::new(),
        }
    }

    /// Declares an entity, its scope, its conflict class and its schema version.
    pub fn register_entity(&mut self, registration: EntityRegistration) {
        self.entities
            .insert(registration.entity.clone(), registration);
    }

    /// Declares which entity a command writes.
    pub fn register_command(&mut self, name: CommandName, entity: EntityName) {
        self.commands.insert(name, entity);
    }

    /// The registration for an entity.
    #[must_use]
    pub fn entity(&self, entity: &EntityName) -> Option<&EntityRegistration> {
        self.entities.get(entity)
    }

    /// The conflict class an entity was registered under.
    #[must_use]
    pub fn class_of(&self, entity: &EntityName) -> Option<ConflictClass> {
        self.entities.get(entity).map(|r| r.conflict_class)
    }

    /// The entity a command writes.
    #[must_use]
    pub fn target_of(&self, name: &CommandName) -> Option<&EntityName> {
        self.commands.get(name)
    }

    /// Every registered entity, in name order.
    pub fn entities(&self) -> impl Iterator<Item = &EntityRegistration> {
        self.entities.values()
    }

    /// Whether this command may be queued at all.
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownCommand`] for an unregistered name,
    /// [`RegistryError::UnknownEntity`] if its target was never declared, and
    /// [`RegistryError::ServerAuthoritative`] if that target is pull-only.
    pub fn check_command(&self, command: &Command) -> Result<&EntityName, RegistryError> {
        let entity =
            self.commands
                .get(&command.name)
                .ok_or_else(|| RegistryError::UnknownCommand {
                    name: command.name.clone(),
                })?;

        let registration =
            self.entities
                .get(entity)
                .ok_or_else(|| RegistryError::UnknownEntity {
                    entity: entity.clone(),
                })?;

        if !registration.conflict_class.accepts_commands() {
            return Err(RegistryError::ServerAuthoritative {
                entity: entity.clone(),
            });
        }

        Ok(entity)
    }
}
