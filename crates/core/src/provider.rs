//! The provider contract every database implementation fulfils.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::model::AppliedMigration;
use crate::planner::PlannedStep;
use crate::source::ScriptFile;

/// Category of a provider error; decides the exit code (SPEC §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// Can't reach the database.
    Connection,
    /// Authentication / authorization failed.
    Auth,
    /// Timed out waiting for the migration lock.
    LockTimeout,
    /// The lock is held and the command won't take it away (`unlock
    /// --force` without `--kill-holder`). Exit 3, like a lock timeout.
    LockHeld,
    /// A migration script failed.
    Execution,
    /// Reading or writing the ledger failed.
    Ledger,
    /// An external runner is missing or too old.
    RunnerUnavailable,
    /// The run was cancelled (Ctrl-C / SIGTERM) and the step was interrupted.
    Cancelled,
    /// Anything else.
    Other,
}

/// An error raised by a provider.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ProviderError {
    /// Category.
    pub kind: ProviderErrorKind,
    /// Human-readable message (must not contain secrets).
    pub message: String,
    /// Underlying error.
    #[source]
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ProviderError {
    /// New error of `kind`.
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    /// Attach an underlying error.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Connection failure.
    pub fn connection(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Connection, message)
    }

    /// Lock timeout; `holder` describes who holds it, if known.
    pub fn lock_timeout(holder: Option<&str>) -> Self {
        let message = match holder {
            Some(h) => format!("timed out waiting for the migration lock (held by {h})"),
            None => "timed out waiting for the migration lock".to_owned(),
        };
        Self::new(ProviderErrorKind::LockTimeout, message)
    }

    /// Script execution failure.
    pub fn execution(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Execution, message)
    }

    /// Ledger failure.
    pub fn ledger(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Ledger, message)
    }

    /// `unlock --force` found the lock held and wasn't allowed to kill.
    pub fn lock_held(holder: Option<&LockHolder>) -> Self {
        let who = holder.map_or_else(|| "an unknown holder".to_owned(), LockHolder::describe);
        Self::new(
            ProviderErrorKind::LockHeld,
            format!(
                "the migration lock is held by {who}; a session lock is released when its \
                 session ends. Wait for that run, or pass --kill-holder to terminate its session"
            ),
        )
    }

    /// `--kill-holder` found the lock held but can't tell, or verify, which
    /// session holds it, so it kills nothing.
    pub fn holder_unverified(reason: &str) -> Self {
        Self::new(
            ProviderErrorKind::LockHeld,
            format!("the migration lock is held, but {reason}; nothing was killed"),
        )
    }
}

/// What a provider can guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// DDL is transactional.
    pub transactional_ddl: bool,
    /// Script and ledger row can commit atomically.
    pub transactional_ledger: bool,
    /// Uses a native database lock (vs. a lease document).
    pub native_lock: bool,
}

/// Options for acquiring the migration lock.
#[derive(Debug, Clone)]
pub struct LockOptions {
    /// How long to wait.
    pub timeout: Duration,
    /// Who is asking; becomes the holder record (SPEC §7 rule 10).
    pub owner: LockOwner,
}

/// The run that asks for the lock. The provider adds the database-side
/// fields when it writes the [`LockHolder`] record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOwner {
    /// OS user and host, e.g. `builder@agent-07`.
    pub os_user: String,
    /// Host name.
    pub host: String,
    /// Process id.
    pub pid: u32,
    /// migrate-db version.
    pub tool_version: String,
    /// CI run reference, e.g. `azure-pipelines:orders#4711`.
    pub ci_run: Option<String>,
}

impl LockOwner {
    /// The current process, described by `identity`.
    pub fn current(identity: &RunIdentity, ci_run: Option<String>) -> Self {
        Self {
            os_user: identity.applied_by.clone(),
            host: identity.host.clone(),
            pid: std::process::id(),
            tool_version: identity.tool_version.clone(),
            ci_run,
        }
    }
}

impl From<&str> for LockOwner {
    /// A labelled owner for tests: `os_user` is the label.
    fn from(label: &str) -> Self {
        Self {
            os_user: label.to_owned(),
            host: "test".into(),
            pid: std::process::id(),
            tool_version: "0.0.0-test".into(),
            ci_run: None,
        }
    }
}

/// Who holds the lock: the record written after acquiring it (SPEC §7
/// rule 10). Session-lock providers store it as JSON in `__migrations_meta`
/// under [`LockHolder::META_KEY`], and remove it on release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockHolder {
    /// The run.
    #[serde(flatten)]
    pub owner: LockOwner,
    /// Database principal, e.g. a login or role name.
    pub principal: String,
    /// Connection label (`application_name` / `APP_NAME`).
    pub application: String,
    /// Database session holding the native lock (`@@SPID`, backend pid).
    pub session_id: Option<i64>,
    /// When the lock was acquired.
    pub acquired_at: DateTime<Utc>,
}

impl LockHolder {
    /// Key of the holder row in the ledger meta table.
    pub const META_KEY: &'static str = "lock_holder";

    /// Serialize for storage.
    pub fn to_record(&self) -> String {
        // INVARIANT: every field serializes; serde_json can't fail here.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse a stored record. `None` if it's unreadable (for example
    /// written by a newer tool): callers treat that as "holder unknown".
    pub fn from_record(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    /// One-line description for messages and logs.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} (principal {}, pid {}",
            self.owner.os_user, self.principal, self.owner.pid
        );
        if let Some(run) = &self.owner.ci_run {
            s.push_str(&format!(", {run}"));
        }
        if let Some(id) = self.session_id {
            s.push_str(&format!(", session {id}"));
        }
        if !self.application.is_empty() {
            s.push_str(&format!(", {}", self.application));
        }
        s.push_str(&format!(
            ", since {})",
            self.acquired_at.format("%Y-%m-%dT%H:%M:%SZ")
        ));
        s
    }
}

/// Read-only view of the migration lock (SPEC §7 rule 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockStatus {
    /// The native lock is held right now.
    pub held: bool,
    /// The holder record, if one exists. When `held` is false it is stale.
    pub holder: Option<LockHolder>,
    /// A holder record exists but the native lock is free.
    pub stale_info: bool,
    /// Lease expiry (lease-lock providers only).
    pub lease_expires_at: Option<DateTime<Utc>>,
}

impl LockStatus {
    /// Combine the native held test with the stored record, following the
    /// stale-record rules of SPEC §7 rule 10.
    pub fn from_parts(held: bool, record: Option<LockHolder>) -> Self {
        Self {
            held,
            stale_info: !held && record.is_some(),
            holder: record,
            lease_expires_at: None,
        }
    }
}

/// What `unlock --force` did (SPEC §4.1, §7 rule 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", content = "holder", rename_all = "kebab-case")]
pub enum UnlockOutcome {
    /// The lock was free and no holder record existed.
    NothingToDo,
    /// The lock was free; the stale holder record was removed (`None`:
    /// the record was unreadable, for example written by a newer tool).
    ClearedStale(Option<LockHolder>),
    /// The holder's database session was terminated and its record removed.
    KilledHolder(LockHolder),
}

/// Who is running migrations; recorded in the ledger.
#[derive(Debug, Clone)]
pub struct RunIdentity {
    /// `applied_by` value.
    pub applied_by: String,
    /// Host name.
    pub host: String,
    /// migrate-db version.
    pub tool_version: String,
}

/// One step to execute.
#[derive(Debug, Clone, Copy)]
pub struct ExecuteRequest<'a> {
    /// The planned step.
    pub step: &'a PlannedStep,
    /// The script to run (the down file for down steps).
    pub script: &'a ScriptFile,
    /// Who is running it.
    pub identity: &'a RunIdentity,
    /// Cancelled on Ctrl-C / SIGTERM. Providers interrupt the running
    /// statement when it fires and report [`ProviderErrorKind::Cancelled`].
    pub cancel: &'a CancelToken,
}

/// Result of a successful step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    /// Execution time.
    pub duration_ms: i64,
}

/// A database provider.
///
/// # `execute` contract
/// * **Transactional step:** run the script **and** insert (up) / delete (down)
///   the ledger row and append a history row in **one transaction**. On
///   failure, roll back and append a failed history row separately.
/// * **Non-transactional step:** run the script; on success write the ledger
///   and history rows; on failure write a `failed` ledger row (SPEC §6.3).
#[async_trait]
pub trait Provider: Send {
    /// Provider name, e.g. `"postgres"`.
    fn kind(&self) -> &'static str;

    /// File extensions this provider executes.
    fn extensions(&self) -> &'static [&'static str];

    /// Guarantees.
    fn capabilities(&self) -> Capabilities;

    /// Offline, provider-specific checks of one script (for example `GO`
    /// lines on SQL Server). Called for every planned step before anything
    /// runs, so a bad file fails the whole run up front. Returns the problem
    /// description. Must not touch the database.
    fn prepare(&self, _script: &ScriptFile) -> Result<(), String> {
        Ok(())
    }

    /// Read-only: is the native lock held, and by whom (SPEC §7 rule 10)?
    /// Must not acquire the lock or create any storage.
    async fn lock_status(&mut self) -> Result<LockStatus, ProviderError>;

    /// Read the applied set. Returns an empty list — and creates nothing —
    /// when the ledger doesn't exist yet (SPEC §7 rule 8).
    async fn read_ledger(&mut self) -> Result<Vec<AppliedMigration>, ProviderError>;

    /// Create the ledger storage if missing. Idempotent; called under the lock.
    async fn ensure_ledger(&mut self) -> Result<(), ProviderError>;

    /// Acquire the target-level exclusive lock, waiting up to `options.timeout`.
    async fn acquire_lock(&mut self, options: &LockOptions) -> Result<(), ProviderError>;

    /// Release the lock.
    async fn release_lock(&mut self) -> Result<(), ProviderError>;

    /// `unlock --force` (SPEC §7 rule 10). Re-checks the lock itself, so a
    /// state change since `lock_status` is handled safely:
    /// * **free:** remove a stale holder record, if any. Only the record that
    ///   was read is removed, never a newer holder's;
    /// * **held, `kill_holder` false:** fail with [`ProviderErrorKind::LockHeld`];
    /// * **held, `kill_holder` true:** terminate the session named in the
    ///   holder record, after verifying that this session really holds the
    ///   lock (fail closed: an unknown or unverifiable holder is never killed),
    ///   then remove the record.
    ///
    /// Records a `force-unlock` history event when anything was done and
    /// the history table exists. Never creates ledger storage.
    async fn force_unlock(
        &mut self,
        kill_holder: bool,
        identity: &RunIdentity,
    ) -> Result<UnlockOutcome, ProviderError>;

    /// Execute one step and update the ledger (see the trait docs).
    async fn execute(&mut self, request: &ExecuteRequest<'_>)
    -> Result<StepOutcome, ProviderError>;

    /// `baseline mark` (SPEC §4.1): write one `applied` ledger row per entry
    /// of `marks` (the `baseline` marker, plus repeatables with
    /// `--mark-repeatables`) and a `baseline` history event for each,
    /// **without executing any script**. Every row commits together where the
    /// database allows it. Called under the lock, after `ensure_ledger`; the
    /// engine has already checked that the ledger is empty.
    async fn mark(
        &mut self,
        marks: &[PlannedStep],
        identity: &RunIdentity,
    ) -> Result<(), ProviderError>;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn holder() -> LockHolder {
        LockHolder {
            owner: LockOwner {
                os_user: "builder@agent-07".into(),
                host: "agent-07".into(),
                pid: 4242,
                tool_version: "0.1.0".into(),
                ci_run: Some("azure-pipelines:orders#4711".into()),
            },
            principal: "deploy_mi".into(),
            application: "migrate-db/0.1.0 azure-pipelines:orders#4711".into(),
            session_id: Some(57),
            acquired_at: DateTime::parse_from_rfc3339("2026-11-18T09:30:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    #[test]
    fn holder_record_round_trips_and_describes_the_run() {
        let h = holder();
        assert_eq!(LockHolder::from_record(&h.to_record()), Some(h.clone()));
        // Flattened owner: one flat JSON object, stable field names.
        let v: serde_json::Value = serde_json::from_str(&h.to_record()).unwrap();
        assert_eq!(v["ci_run"], "azure-pipelines:orders#4711");
        assert_eq!(v["session_id"], 57);
        assert_eq!(LockHolder::from_record("not json"), None);
        let d = h.describe();
        for part in [
            "builder@agent-07",
            "deploy_mi",
            "#4711",
            "session 57",
            "2026-11-18T09:30:00Z",
        ] {
            assert!(d.contains(part), "{d}");
        }
    }

    #[test]
    fn stale_record_rules() {
        // (held, record) -> stale_info
        for (held, record, stale) in [
            (true, Some(holder()), false),
            (true, None, false), // held, holder unknown
            (false, Some(holder()), true),
            (false, None, false),
        ] {
            let s = LockStatus::from_parts(held, record.clone());
            assert_eq!((s.held, s.stale_info, s.holder), (held, stale, record));
        }
    }
}
