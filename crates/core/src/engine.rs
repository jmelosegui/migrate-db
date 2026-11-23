//! The engine: orchestrates provider, planner and lock (ARCHITECTURE §4).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cancel::CancelToken;
use crate::error::{Error, exit};
use crate::model::Direction;
use crate::placeholders::{self, Values};
use crate::planner::{self, Plan, PlannedStep, Policy, Request, StatusReport, StepId};
use crate::provider::{
    ExecuteRequest, LockOptions, LockStatus, Provider, ProviderError, ProviderErrorKind,
    RunIdentity, StepOutcome, UnlockOutcome,
};
use crate::source::{MigrationSet, ScriptFile};
use crate::version::Version;

/// Receives progress events.
pub trait Observer: Send + Sync {
    /// A step is about to run.
    fn step_started(&self, _step: &PlannedStep) {}
    /// A step finished.
    fn step_finished(&self, _step: &PlannedStep, _result: &Result<StepOutcome, ProviderError>) {}
    /// Still waiting for the lock after `waited`. `lock` is the current
    /// holder, or `None` if it couldn't be read (SPEC §7 rule 10).
    fn lock_waiting(&self, _waited: Duration, _lock: Option<&LockStatus>) {}
}

/// How often a run waiting for the lock reports the holder (SPEC §7 rule 10).
pub const LOCK_REPORT_INTERVAL: Duration = Duration::from_secs(15);

/// Observer that ignores everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopObserver;

impl Observer for NoopObserver {}

/// A completed step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepResult {
    /// Version.
    pub version: StepId,
    /// Description.
    pub description: String,
    /// Direction.
    pub direction: Direction,
    /// Execution time.
    pub duration_ms: i64,
}

/// Why a step failed; decides the run's exit code (SPEC §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The script failed (exit 4).
    Execution,
    /// The connection was lost or refused (exit 5). The step's outcome may
    /// be unknown: its commit may or may not have reached the server.
    Connection,
    /// Authentication failed (exit 5).
    Auth,
    /// An external runner is missing or too old (exit 6).
    Runner,
    /// Writing the ledger failed, or anything else (exit 1).
    Internal,
    /// The run was cancelled (Ctrl-C / SIGTERM) and this step was
    /// interrupted and rolled back (exit 4, SPEC §7 rule 7).
    Cancelled,
}

impl FailureKind {
    fn from_provider(kind: ProviderErrorKind) -> Self {
        match kind {
            ProviderErrorKind::Execution => Self::Execution,
            ProviderErrorKind::Connection => Self::Connection,
            ProviderErrorKind::Auth => Self::Auth,
            ProviderErrorKind::RunnerUnavailable => Self::Runner,
            ProviderErrorKind::Cancelled => Self::Cancelled,
            ProviderErrorKind::LockTimeout
            | ProviderErrorKind::LockHeld
            | ProviderErrorKind::Ledger
            | ProviderErrorKind::Other => Self::Internal,
        }
    }

    /// The exit code for a run that stopped with this failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Execution | Self::Cancelled => exit::MIGRATION_FAILED,
            Self::Connection | Self::Auth => exit::CONNECTION,
            Self::Runner => exit::RUNNER,
            Self::Internal => exit::INTERNAL,
        }
    }
}

/// The step that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepFailure {
    /// Version.
    pub version: StepId,
    /// Description.
    pub description: String,
    /// Direction.
    pub direction: Direction,
    /// What kind of failure.
    pub kind: FailureKind,
    /// Error message.
    pub error: String,
}

/// Outcome of a mutating run.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// The plan made under the lock.
    pub plan: Plan,
    /// Steps that completed, in order.
    pub completed: Vec<StepResult>,
    /// The step that failed (the run stopped there).
    pub failure: Option<StepFailure>,
    /// The run was cancelled between steps: the rest of the plan didn't run.
    pub cancelled: bool,
    /// Set if releasing the lock failed after the run.
    pub lock_release_error: Option<String>,
}

impl RunReport {
    /// `true` if every planned step completed.
    pub fn succeeded(&self) -> bool {
        self.failure.is_none() && !self.cancelled
    }

    /// The process exit code for this run (SPEC §4.2).
    pub fn exit_code(&self) -> i32 {
        if let Some(f) = &self.failure {
            return f.kind.exit_code();
        }
        if self.cancelled {
            exit::MIGRATION_FAILED
        } else {
            exit::OK
        }
    }
}

/// Outcome of `baseline mark`: the marker rows written. Nothing was executed.
#[derive(Debug, Clone, Serialize)]
pub struct MarkReport {
    /// The baseline plan made under the lock (its steps are the rows written).
    pub plan: Plan,
    /// Set if releasing the lock failed after the rows were committed.
    pub lock_release_error: Option<String>,
}

/// What `baseline mark` writes (SPEC §4.1).
#[derive(Debug, Clone, Copy)]
pub struct BaselineRequest<'a> {
    /// The baseline version: an existing `V<version>__...` file.
    pub version: &'a Version,
    /// Overrides the file's description in the marker row.
    pub description: Option<&'a str>,
    /// Also record every repeatable at its current checksum.
    pub mark_repeatables: bool,
}

/// Runs plans against one provider.
pub struct Engine<'a> {
    provider: &'a mut dyn Provider,
    set: &'a MigrationSet,
    policy: Policy,
    identity: RunIdentity,
    /// Placeholder values when placeholders are enabled (SPEC §3.3).
    placeholders: Option<Values>,
    /// Cancelled on Ctrl-C / SIGTERM (SPEC §7 rule 7).
    cancel: CancelToken,
    /// How often the holder is reported while waiting for the lock.
    lock_report_interval: Duration,
}

impl std::fmt::Debug for Engine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("provider", &self.provider.kind())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl<'a> Engine<'a> {
    /// New engine.
    pub fn new(
        provider: &'a mut dyn Provider,
        set: &'a MigrationSet,
        policy: Policy,
        identity: RunIdentity,
    ) -> Self {
        Self {
            provider,
            set,
            policy,
            identity,
            placeholders: None,
            cancel: CancelToken::new(),
            lock_report_interval: LOCK_REPORT_INTERVAL,
        }
    }

    /// Stop the run when `token` is cancelled: no new step starts, the
    /// running statement is interrupted and rolled back, the lock is
    /// released (SPEC §7 rule 7).
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = token;
        self
    }

    /// Report the holder this often while waiting for the lock (default
    /// [`LOCK_REPORT_INTERVAL`]; shorter in tests).
    pub fn with_lock_report_interval(mut self, every: Duration) -> Self {
        self.lock_report_interval = every.max(Duration::from_millis(1));
        self
    }

    /// Enable `${name}` substitution with these values (SPEC §3.3). Without
    /// this, scripts run exactly as written.
    pub fn with_placeholders(mut self, values: Option<Values>) -> Self {
        self.placeholders = values;
        self
    }

    /// The script as it will run: placeholders substituted, if enabled.
    fn rendered(&self, script: &ScriptFile) -> Result<ScriptFile, String> {
        let Some(values) = &self.placeholders else {
            return Ok(script.clone());
        };
        let content = placeholders::render(&script.content, values)
            .map_err(|e| format!("{}: {e}", script.relative_path))?;
        Ok(ScriptFile {
            content,
            ..script.clone()
        })
    }

    /// Read-only status. Takes no lock and creates nothing.
    pub async fn status(&mut self) -> Result<StatusReport, Error> {
        let ledger = self.provider.read_ledger().await?;
        Ok(planner::status(self.set, &ledger)?)
    }

    /// Read-only plan (`--dry-run`). Takes no lock and creates nothing.
    /// Fails if any planned script is rejected by the provider's offline checks.
    pub async fn dry_run(&mut self, request: &Request) -> Result<Plan, Error> {
        let ledger = self.provider.read_ledger().await?;
        let plan = planner::plan(self.set, &ledger, request, self.policy)?;
        self.prepare(&plan)?;
        Ok(plan)
    }

    /// Read-only plan for `baseline mark --dry-run`. Takes no lock and
    /// creates nothing.
    pub async fn dry_run_baseline(&mut self, request: &BaselineRequest<'_>) -> Result<Plan, Error> {
        let ledger = self.provider.read_ledger().await?;
        Ok(planner::baseline_plan(
            self.set,
            &ledger,
            request.version,
            request.description,
            request.mark_repeatables,
        )?)
    }

    /// `baseline mark` (SPEC §4.1): lock → ensure ledger → read (must be
    /// empty) → plan → (verify) → write the marker rows → release. **No
    /// script is executed.** Returns `Err` when nothing was written.
    pub async fn mark_baseline(
        &mut self,
        request: &BaselineRequest<'_>,
        lock: &LockOptions,
        expect_fingerprint: Option<&str>,
        observer: &dyn Observer,
    ) -> Result<MarkReport, Error> {
        self.acquire(lock, observer).await?;
        if self.cancel.is_cancelled() {
            let _ = self.provider.release_lock().await;
            return Err(Error::Cancelled);
        }
        let result = self.mark_locked(request, expect_fingerprint).await;
        let release = self.provider.release_lock().await;
        let plan = result?;
        Ok(MarkReport {
            plan,
            lock_release_error: release.err().map(|e| e.to_string()),
        })
    }

    async fn mark_locked(
        &mut self,
        request: &BaselineRequest<'_>,
        expect_fingerprint: Option<&str>,
    ) -> Result<Plan, Error> {
        self.provider.ensure_ledger().await?;
        let ledger = self.provider.read_ledger().await?;
        let plan = planner::baseline_plan(
            self.set,
            &ledger,
            request.version,
            request.description,
            request.mark_repeatables,
        )?;
        if let Some(expected) = expect_fingerprint
            && expected != plan.fingerprint
        {
            return Err(Error::PlanMismatch {
                expected: expected.to_owned(),
                actual: plan.fingerprint,
            });
        }
        self.provider.mark(&plan.steps, &self.identity).await?;
        Ok(plan)
    }

    /// Read-only lock state (SPEC §7 rule 10).
    pub async fn lock_status(&mut self) -> Result<LockStatus, Error> {
        Ok(self.provider.lock_status().await?)
    }

    /// Render placeholders and run the provider's offline checks on every
    /// planned script, so a bad file or a missing placeholder value stops
    /// the run before its first step (SPEC §7 rule 4).
    fn prepare(&self, plan: &Plan) -> Result<(), Error> {
        let mut problems = Vec::new();
        for step in &plan.steps {
            match self.rendered(self.script_for(step)?) {
                Ok(script) => {
                    if let Err(p) = self.provider.prepare(&script) {
                        problems.push(p);
                    }
                }
                Err(p) => problems.push(p),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidScripts(problems))
        }
    }

    /// Mutating run: lock → ensure ledger → read → plan → (verify) → execute → release.
    ///
    /// Returns `Err` only when nothing was executed (lock, ledger, plan or
    /// fingerprint problems). A failing step returns `Ok` with
    /// [`RunReport::failure`] set.
    pub async fn run(
        &mut self,
        request: &Request,
        lock: &LockOptions,
        expect_fingerprint: Option<&str>,
        observer: &dyn Observer,
    ) -> Result<RunReport, Error> {
        self.acquire(lock, observer).await?;
        if self.cancel.is_cancelled() {
            let _ = self.provider.release_lock().await;
            return Err(Error::Cancelled);
        }
        let result = self.run_locked(request, expect_fingerprint, observer).await;
        let release = self.provider.release_lock().await;
        match result {
            Ok(mut report) => {
                report.lock_release_error = release.err().map(|e| e.to_string());
                Ok(report)
            }
            Err(e) => Err(e),
        }
    }

    /// Wait for the lock in slices of `lock_report_interval`, reporting the
    /// holder between slices, until `lock.timeout` (SPEC §7 rule 10). The
    /// last slice's timeout error names the holder.
    async fn acquire(&mut self, lock: &LockOptions, observer: &dyn Observer) -> Result<(), Error> {
        let started = Instant::now();
        let deadline = started + lock.timeout;
        loop {
            let slice = LockOptions {
                timeout: deadline
                    .saturating_duration_since(Instant::now())
                    .min(self.lock_report_interval),
                owner: lock.owner.clone(),
            };
            // A cancel while waiting abandons the wait. Dropping the request
            // closes its connection, so the server stops waiting too.
            let cancel = self.cancel.clone();
            let attempt = tokio::select! {
                acquired = self.provider.acquire_lock(&slice) => acquired,
                () = cancel.cancelled() => return Err(Error::Cancelled),
            };
            match attempt {
                Ok(()) => return Ok(()),
                Err(e) if e.kind == ProviderErrorKind::LockTimeout && Instant::now() < deadline => {
                    let status = self.provider.lock_status().await.ok();
                    observer.lock_waiting(started.elapsed(), status.as_ref());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// `unlock --force [--kill-holder]` (SPEC §4.1). Takes no lock: the
    /// provider re-checks the lock and acts only on what it verified.
    pub async fn force_unlock(&mut self, kill_holder: bool) -> Result<UnlockOutcome, Error> {
        Ok(self
            .provider
            .force_unlock(kill_holder, &self.identity)
            .await?)
    }

    async fn run_locked(
        &mut self,
        request: &Request,
        expect_fingerprint: Option<&str>,
        observer: &dyn Observer,
    ) -> Result<RunReport, Error> {
        self.provider.ensure_ledger().await?;
        let ledger = self.provider.read_ledger().await?;
        let plan = planner::plan(self.set, &ledger, request, self.policy)?;
        if let Some(expected) = expect_fingerprint
            && expected != plan.fingerprint
        {
            return Err(Error::PlanMismatch {
                expected: expected.to_owned(),
                actual: plan.fingerprint,
            });
        }
        self.prepare(&plan)?;

        let mut completed = Vec::with_capacity(plan.steps.len());
        let mut failure = None;
        let mut cancelled = false;
        for step in &plan.steps {
            if self.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            // `prepare` already rendered every script, so this can't fail
            // here; map it anyway rather than unwrap.
            let script = self
                .rendered(self.script_for(step)?)
                .map_err(|p| Error::InvalidScripts(vec![p]))?;
            observer.step_started(step);
            let mut result = self
                .provider
                .execute(&ExecuteRequest {
                    step,
                    script: &script,
                    identity: &self.identity,
                    cancel: &self.cancel,
                })
                .await;
            // An error caused by the interruption is a cancellation, whatever
            // the driver called it.
            if let Err(e) = &mut result
                && self.cancel.is_cancelled()
                && e.kind != ProviderErrorKind::Cancelled
            {
                e.kind = ProviderErrorKind::Cancelled;
                e.message = format!("cancelled: {}", e.message);
            }
            observer.step_finished(step, &result);
            match result {
                Ok(outcome) => completed.push(StepResult {
                    version: step.version.clone(),
                    description: step.description.clone(),
                    direction: step.direction,
                    duration_ms: outcome.duration_ms,
                }),
                // Any failure stops the run, but the report keeps the steps
                // that already committed, so the output says what happened.
                Err(e) => {
                    let kind = FailureKind::from_provider(e.kind);
                    let mut error = e.to_string();
                    if kind == FailureKind::Connection {
                        error.push_str(
                            "; the connection was lost during this step, so its outcome is unknown: \
                             run `migrate-db status` to see what the database recorded",
                        );
                    }
                    failure = Some(StepFailure {
                        version: step.version.clone(),
                        description: step.description.clone(),
                        direction: step.direction,
                        kind,
                        error,
                    });
                    break;
                }
            }
        }
        Ok(RunReport {
            plan,
            completed,
            failure,
            cancelled,
            lock_release_error: None,
        })
    }

    fn script_for(&self, step: &PlannedStep) -> Result<&'a ScriptFile, Error> {
        let set: &'a MigrationSet = self.set;
        let version = match &step.version {
            StepId::Version(v) => v,
            StepId::Repeatable(_) => {
                let name = step.version.repeatable_name().unwrap_or_default();
                return set
                    .repeatables
                    .iter()
                    .find(|r| r.name == name)
                    .map(|r| &r.script)
                    .ok_or_else(|| {
                        Error::Usage(format!(
                            "internal: no file for planned step {}",
                            step.version
                        ))
                    });
            }
        };
        let m = set.get(version).ok_or_else(|| {
            Error::Usage(format!(
                "internal: no file for planned step {}",
                step.version
            ))
        })?;
        match step.direction {
            Direction::Up => Ok(&m.up),
            Direction::Down => m.down.as_ref().ok_or_else(|| {
                Error::Usage(format!("internal: no down file for {}", step.version))
            }),
        }
    }
}
