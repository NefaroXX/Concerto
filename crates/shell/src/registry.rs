use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::{CommandSpec, ShellCommand};

/// Command registration or catalog access failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("invalid command name `{0}`; use lowercase ASCII letters, digits, and hyphens")]
    InvalidName(String),
    #[error("command `{0}` is already registered")]
    Duplicate(String),
    #[error("command registry lock is unavailable")]
    Unavailable,
}

/// Thread-safe command catalog used by runtimes and help rendering.
#[derive(Clone, Default)]
pub struct CommandRegistry {
    commands: Arc<RwLock<BTreeMap<String, Arc<dyn ShellCommand>>>>,
}

impl CommandRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a command by its declared name.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name, a duplicate, or an unavailable
    /// registry lock.
    pub fn register(&self, command: Arc<dyn ShellCommand>) -> Result<(), RegistryError> {
        let name = command.spec().name.clone();
        if !is_valid_command_name(&name) {
            return Err(RegistryError::InvalidName(name));
        }

        let mut commands = self.commands.write().map_err(|_| RegistryError::Unavailable)?;
        if commands.contains_key(&name) {
            return Err(RegistryError::Duplicate(name));
        }
        commands.insert(name, command);
        Ok(())
    }

    /// Resolve a command by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lock is unavailable.
    pub fn get(&self, name: &str) -> Result<Option<Arc<dyn ShellCommand>>, RegistryError> {
        let commands = self.commands.read().map_err(|_| RegistryError::Unavailable)?;
        Ok(commands.get(name).cloned())
    }

    /// Return command specifications in deterministic name order.
    ///
    /// # Errors
    ///
    /// Returns an error if the registry lock is unavailable.
    pub fn specs(&self) -> Result<Vec<CommandSpec>, RegistryError> {
        let commands = self.commands.read().map_err(|_| RegistryError::Unavailable)?;
        Ok(commands.values().map(|command| command.spec().clone()).collect())
    }
}

fn is_valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::is_valid_command_name;

    #[test]
    fn validates_stable_command_names() {
        assert!(is_valid_command_name("ls-tree"));
        assert!(is_valid_command_name("help2"));
        assert!(!is_valid_command_name(""));
        assert!(!is_valid_command_name("ProjectInfo"));
        assert!(!is_valid_command_name("-hidden"));
        assert!(!is_valid_command_name("two words"));
    }
}
