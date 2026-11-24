//! Recording integrity is independent of whether a task completed its work.

use serde::{Deserialize, Serialize};

use crate::ids::Ulid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecorderKind {
    EventLog,
    Transcript,
}

impl RecorderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::EventLog => "event log",
            Self::Transcript => "transcript",
        }
    }
}

/// One recorder's terminal health. A missing outcome is unknown, never complete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecorderOutcome {
    pub recording_id: Ulid,
    pub recorder: RecorderKind,
    pub dropped_events: u64,
    /// Failed event writes or transcript batch writes (not a count of rows).
    pub failed_writes: u64,
    /// The recorder worker failed or did not drain before its deadline. This
    /// says nothing about whether the agent completed the requested task.
    pub recorder_failed: bool,
    /// Whether this outcome was itself saved outside the recorder's queue.
    pub report_persisted: bool,
}

impl RecorderOutcome {
    pub fn is_complete(&self) -> bool {
        self.dropped_events == 0
            && self.failed_writes == 0
            && !self.recorder_failed
            && self.report_persisted
    }

    pub fn warning(&self) -> Option<String> {
        if self.is_complete() {
            return None;
        }
        let mut problems = Vec::new();
        if self.dropped_events > 0 {
            problems.push(format!("lost {} events", self.dropped_events));
        }
        if self.failed_writes > 0 {
            problems.push(format!("{} writes failed", self.failed_writes));
        }
        if self.recorder_failed {
            problems.push("recorder stopped unexpectedly".into());
        }
        if !self.report_persisted {
            problems.push("integrity status could not be saved".into());
        }
        Some(format!("{} ({})", self.recorder.label(), problems.join(", ")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordingIntegrity {
    pub event_log: RecorderOutcome,
    pub transcript: RecorderOutcome,
}

impl RecordingIntegrity {
    pub fn is_complete(&self) -> bool {
        self.event_log.is_complete() && self.transcript.is_complete()
    }

    pub fn warning(&self) -> Option<String> {
        let details: Vec<_> = [&self.event_log, &self.transcript]
            .into_iter()
            .filter_map(RecorderOutcome::warning)
            .collect();
        (!details.is_empty()).then(|| format!("Recording incomplete: {}.", details.join("; ")))
    }
}
