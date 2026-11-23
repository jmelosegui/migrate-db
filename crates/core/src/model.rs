//! Shared model types: directions and ledger rows (SPEC §6).

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Direction of a migration step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Apply.
    Up,
    /// Revert.
    Down,
}

impl Direction {
    /// `"up"` or `"down"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Kind of a ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A versioned migration.
    Versioned,
    /// A baseline marker.
    Baseline,
    /// A repeatable migration.
    Repeatable,
}

impl Kind {
    /// Ledger text value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Versioned => "versioned",
            Self::Baseline => "baseline",
            Self::Repeatable => "repeatable",
        }
    }

    /// Parse the ledger text value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "versioned" => Some(Self::Versioned),
            "baseline" => Some(Self::Baseline),
            "repeatable" => Some(Self::Repeatable),
            _ => None,
        }
    }
}

/// Status of a ledger row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Fully applied.
    Applied,
    /// A non-transactional run is (or was, if it crashed) in progress.
    InProgress,
    /// A non-transactional run failed part-way.
    Failed,
}

impl Status {
    /// Ledger text value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::InProgress => "in_progress",
            Self::Failed => "failed",
        }
    }

    /// Parse the ledger text value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "applied" => Some(Self::Applied),
            "in_progress" => Some(Self::InProgress),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One row of the applied set (SPEC §6.1).
///
/// `version` is raw text: a numeric version for `versioned` / `baseline`
/// rows, `R__<name>` for repeatables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedMigration {
    /// Version text (or `R__<name>`).
    pub version: String,
    /// Description.
    pub description: String,
    /// Up file name.
    pub script_name: String,
    /// SHA-256 hex of the normalized up script.
    pub checksum: String,
    /// Row kind.
    pub kind: Kind,
    /// Row status.
    pub status: Status,
    /// Direction of an in-progress / failed run.
    pub direction: Option<Direction>,
    /// When it was applied.
    pub applied_at: DateTime<Utc>,
    /// Who applied it.
    pub applied_by: String,
    /// Execution time in milliseconds.
    pub execution_ms: i64,
    /// migrate-db version that applied it.
    pub tool_version: String,
    /// `native` / `sqlcmd` / `mongosh`.
    pub executor: String,
    /// External runner version, if any.
    pub runner_version: Option<String>,
    /// Operation count for operation-list files (SPEC §6.5).
    pub op_count: Option<i32>,
    /// Completed operations.
    pub completed_ops: Option<i32>,
    /// Failed operation index.
    pub failed_op: Option<i32>,
    /// Recovery policy captured at run start.
    pub recovery: Option<String>,
    /// Lease epoch that owns an in-progress / failed row.
    pub lock_epoch: Option<i64>,
}

/// History event type (SPEC §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    /// Up step.
    Up,
    /// Down step.
    Down,
    /// Baseline marker written.
    Baseline,
    /// Ledger repaired.
    Repair,
    /// Lock force-released.
    ForceUnlock,
    /// Operation-level resume.
    Resume,
    /// Recovery rollback.
    RecoveryRollback,
}

impl EventKind {
    /// Ledger text value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Baseline => "baseline",
            Self::Repair => "repair",
            Self::ForceUnlock => "force-unlock",
            Self::Resume => "resume",
            Self::RecoveryRollback => "recovery-rollback",
        }
    }
}

/// One history-log row (SPEC §6.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// Version text.
    pub version: String,
    /// Direction, when relevant.
    pub direction: Option<Direction>,
    /// Event type.
    pub event: EventKind,
    /// `true` = succeeded.
    pub succeeded: bool,
    /// Checksum at the time.
    pub checksum: Option<String>,
    /// Start.
    pub started_at: DateTime<Utc>,
    /// End.
    pub finished_at: DateTime<Utc>,
    /// Who.
    pub applied_by: String,
    /// Host.
    pub host: String,
    /// migrate-db version.
    pub tool_version: String,
    /// Truncated error message.
    pub error: Option<String>,
}

/// Maximum length of an error message stored in the history log.
pub const MAX_ERROR_LEN: usize = 4000;

/// Truncate an error message for the history log, on a char boundary.
pub fn truncate_error(message: &str) -> String {
    if message.len() <= MAX_ERROR_LEN {
        return message.to_owned();
    }
    let mut end = MAX_ERROR_LEN;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_values_round_trip() {
        for k in [Kind::Versioned, Kind::Baseline, Kind::Repeatable] {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
        }
        for s in [Status::Applied, Status::InProgress, Status::Failed] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert_eq!(Kind::parse("nope"), None);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let long = "é".repeat(MAX_ERROR_LEN);
        let t = truncate_error(&long);
        assert!(t.len() <= MAX_ERROR_LEN + "…".len());
        assert!(t.ends_with('…'));
        assert_eq!(truncate_error("short"), "short");
    }
}
