use concerto_core::error::PolicyError;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::CancellationToken;
use std::sync::{Arc, Mutex};

/// In-memory audit log for testing. Records entries in a Vec.
#[cfg(test)]
pub struct InMemoryAuditLog {
    entries: Arc<Mutex<Vec<AuditEntry>>>,
}

#[cfg(test)]
impl InMemoryAuditLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub fn entry_count(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
impl Default for InMemoryAuditLog {
    fn default() -> Self {
        Self { entries: Arc::new(Mutex::new(Vec::new())) }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl AuditLog for InMemoryAuditLog {
    async fn record(
        &self,
        entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        // Cancellation checked at statement boundaries; single-statement fast path.
        self.entries
            .lock()
            .map_err(|e| PolicyError::AuditLogWriteFailed(e.to_string()))?
            .push(entry);
        Ok(())
    }
}
