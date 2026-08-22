use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::CommandResult;

/// Failure to access in-memory shell history.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HistoryError {
    #[error("shell history lock is unavailable")]
    Unavailable,
}

/// Bounded in-memory result history shared by runtime meta-commands.
#[derive(Clone, Debug)]
pub struct ShellHistory {
    capacity: usize,
    entries: Arc<RwLock<VecDeque<CommandResult>>>,
}

impl ShellHistory {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), entries: Arc::new(RwLock::new(VecDeque::new())) }
    }

    /// Add a result, evicting the oldest result at capacity.
    ///
    /// # Errors
    ///
    /// Returns an error if another thread poisoned the history lock.
    pub fn record(&self, result: CommandResult) -> Result<(), HistoryError> {
        let mut entries = self.entries.write().map_err(|_| HistoryError::Unavailable)?;
        if entries.len() == self.capacity {
            entries.pop_front();
        }
        entries.push_back(result);
        Ok(())
    }

    /// Return the most recent recorded result.
    ///
    /// # Errors
    ///
    /// Returns an error if another thread poisoned the history lock.
    pub fn last(&self) -> Result<Option<CommandResult>, HistoryError> {
        let entries = self.entries.read().map_err(|_| HistoryError::Unavailable)?;
        Ok(entries.back().cloned())
    }

    /// Number of retained results.
    ///
    /// # Errors
    ///
    /// Returns an error if another thread poisoned the history lock.
    pub fn len(&self) -> Result<usize, HistoryError> {
        let entries = self.entries.read().map_err(|_| HistoryError::Unavailable)?;
        Ok(entries.len())
    }

    /// Whether no results have been recorded.
    ///
    /// # Errors
    ///
    /// Returns an error if another thread poisoned the history lock.
    pub fn is_empty(&self) -> Result<bool, HistoryError> {
        self.len().map(|length| length == 0)
    }
}
