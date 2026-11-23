//! Top-level error type and the exit-code contract (SPEC §4.2).

use crate::config::ConfigError;
use crate::planner::PlanError;
use crate::provider::{ProviderError, ProviderErrorKind};
use crate::source::SourceErrors;

/// Exit codes. **Public contract** — don't renumber.
pub mod exit {
    /// Success, including "nothing to do".
    pub const OK: i32 = 0;
    /// Unexpected / internal error.
    pub const INTERNAL: i32 = 1;
    /// Validation failed (drift, checksums, bad files, out-of-order…).
    pub const VALIDATION: i32 = 2;
    /// Timed out acquiring the lock.
    pub const LOCK_TIMEOUT: i32 = 3;
    /// A migration failed during execution.
    pub const MIGRATION_FAILED: i32 = 4;
    /// Connection or authentication failure.
    pub const CONNECTION: i32 = 5;
    /// External runner missing or too old.
    pub const RUNNER: i32 = 6;
    /// CLI usage error.
    pub const USAGE: i32 = 64;
}

/// Any error the engine or CLI can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration problem.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Migration files problem.
    #[error("{0}")]
    Source(#[from] SourceErrors),
    /// Planning / reconciliation problem.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// `--expect-plan` didn't match the plan made under the lock.
    #[error(
        "the plan changed since it was approved (expected fingerprint {expected}, got {actual}); nothing was executed"
    )]
    PlanMismatch {
        /// Approved fingerprint.
        expected: String,
        /// Current fingerprint.
        actual: String,
    },
    /// Provider failure.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Scripts rejected by the provider's offline checks (`prepare`).
    #[error("invalid migration script(s); nothing was executed: {}", .0.join("; "))]
    InvalidScripts(Vec<String>),
    /// Cancelled before any step ran (for example while waiting for the lock).
    #[error("cancelled; nothing was executed")]
    Cancelled,
    /// `baseline mark` could not check the live schema against a captured
    /// fingerprint, and the run did not pass `--force-unverified`.
    #[error("{0}")]
    BaselineUnverified(String),
    /// CLI usage problem.
    #[error("{0}")]
    Usage(String),
}

impl Error {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(e) if e.is_not_found() => exit::USAGE,
            Self::Config(_)
            | Self::Source(_)
            | Self::PlanMismatch { .. }
            | Self::InvalidScripts(_)
            | Self::BaselineUnverified(_) => exit::VALIDATION,
            Self::Plan(e) if e.is_usage() => exit::USAGE,
            Self::Plan(_) => exit::VALIDATION,
            Self::Usage(_) => exit::USAGE,
            // SPEC §7 rule 7: a cancelled run exits with 4.
            Self::Cancelled => exit::MIGRATION_FAILED,
            Self::Provider(e) => match e.kind {
                ProviderErrorKind::Connection | ProviderErrorKind::Auth => exit::CONNECTION,
                ProviderErrorKind::LockTimeout | ProviderErrorKind::LockHeld => exit::LOCK_TIMEOUT,
                ProviderErrorKind::Execution => exit::MIGRATION_FAILED,
                ProviderErrorKind::RunnerUnavailable => exit::RUNNER,
                ProviderErrorKind::Cancelled => exit::MIGRATION_FAILED,
                ProviderErrorKind::Ledger | ProviderErrorKind::Other => exit::INTERNAL,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exit_codes_follow_the_spec() {
        let not_found = Error::Config(ConfigError::NotFound {
            start: PathBuf::from("."),
        });
        assert_eq!(not_found.exit_code(), exit::USAGE);
        assert_eq!(
            Error::Plan(PlanError::DownNotAllowed).exit_code(),
            exit::VALIDATION
        );
        assert_eq!(
            Error::Plan(PlanError::ConflictingLimits).exit_code(),
            exit::USAGE
        );
        assert_eq!(
            Error::Provider(ProviderError::lock_timeout(None)).exit_code(),
            exit::LOCK_TIMEOUT
        );
        assert_eq!(
            Error::Provider(ProviderError::connection("x")).exit_code(),
            exit::CONNECTION
        );
        assert_eq!(
            Error::Provider(ProviderError::execution("x")).exit_code(),
            exit::MIGRATION_FAILED
        );
        assert_eq!(
            Error::PlanMismatch {
                expected: "a".into(),
                actual: "b".into()
            }
            .exit_code(),
            exit::VALIDATION
        );
        assert_eq!(
            Error::InvalidScripts(vec!["x".into()]).exit_code(),
            exit::VALIDATION
        );
    }
}
