//! In-memory provider used to test the engine. Not shipped.
//!
//! A [`MemoryDatabase`] is shared state (ledger, history, lock); every
//! [`MemoryProvider`] is one "connection" to it, so lock contention between
//! two runs can be tested.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;

use migrate_db_core::model::{AppliedMigration, Direction, EventKind, HistoryEvent, Status};
use migrate_db_core::planner::PlannedStep;
use migrate_db_core::provider::{
    Capabilities, ExecuteRequest, LockHolder, LockOptions, LockStatus, Provider, ProviderError,
    RunIdentity, StepOutcome, UnlockOutcome,
};
use migrate_db_core::source::ScriptFile;

#[derive(Debug, Default)]
struct State {
    ledger: Option<Vec<AppliedMigration>>,
    history: Vec<HistoryEvent>,
    lock_holder: Option<(u64, LockHolder)>,
    /// A holder record left behind (simulates a crashed session-lock run).
    stale_holder: Option<LockHolder>,
    reject: BTreeSet<String>,
    executed: Vec<(Direction, String)>,
    /// Text of every script that ran successfully, as the provider received it.
    scripts: Vec<String>,
    fail_on: BTreeSet<(Direction, String)>,
    lose_connection_on: BTreeSet<(Direction, String)>,
}

/// Shared in-memory "database".
#[derive(Debug, Clone, Default)]
pub struct MemoryDatabase {
    state: Arc<Mutex<State>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl MemoryDatabase {
    /// New empty database (no ledger yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// A new connection.
    pub fn connect(&self) -> MemoryProvider {
        MemoryProvider {
            db: self.clone(),
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Make the step `(direction, version)` fail.
    pub fn fail_on(&self, direction: Direction, version: &str) {
        self.lock().fail_on.insert((direction, version.to_owned()));
    }

    /// Make the step `(direction, version)` lose the connection: a
    /// transactional step leaves no trace, as when the server rolls back.
    pub fn lose_connection_on(&self, direction: Direction, version: &str) {
        self.lock()
            .lose_connection_on
            .insert((direction, version.to_owned()));
    }

    /// Stop failing all steps.
    pub fn clear_failures(&self) {
        let mut state = self.lock();
        state.fail_on.clear();
        state.lose_connection_on.clear();
    }

    /// `true` once `ensure_ledger` has run.
    pub fn ledger_exists(&self) -> bool {
        self.lock().ledger.is_some()
    }

    /// Current ledger rows.
    pub fn ledger(&self) -> Vec<AppliedMigration> {
        self.lock().ledger.clone().unwrap_or_default()
    }

    /// Mutate the ledger directly (to simulate drift or a crash).
    pub fn edit_ledger(&self, f: impl FnOnce(&mut Vec<AppliedMigration>)) {
        let mut state = self.lock();
        f(state.ledger.get_or_insert_with(Vec::new));
    }

    /// History rows.
    pub fn history(&self) -> Vec<HistoryEvent> {
        self.lock().history.clone()
    }

    /// Every step whose script ran successfully, in order.
    pub fn executed(&self) -> Vec<(Direction, String)> {
        self.lock().executed.clone()
    }

    /// The text of every script that ran successfully, as received (after
    /// placeholder substitution).
    pub fn executed_scripts(&self) -> Vec<String> {
        self.lock().scripts.clone()
    }

    /// Current lock holder record.
    pub fn lock_holder(&self) -> Option<LockHolder> {
        self.lock().lock_holder.as_ref().map(|(_, h)| h.clone())
    }

    /// Leave a holder record without holding the lock, like a session-lock
    /// run whose process died before it could remove the record.
    pub fn leave_stale_holder(&self, holder: LockHolder) {
        self.lock().stale_holder = Some(holder);
    }

    /// Make `prepare` reject the script with this relative path.
    pub fn reject_script(&self, relative_path: &str) {
        self.lock().reject.insert(relative_path.to_owned());
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A poisoned mutex only means a test panicked mid-update; keep going.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One connection to a [`MemoryDatabase`].
#[derive(Debug)]
pub struct MemoryProvider {
    db: MemoryDatabase,
    id: u64,
}

fn history(request: &ExecuteRequest<'_>, succeeded: bool, error: Option<String>) -> HistoryEvent {
    let now = Utc::now();
    HistoryEvent {
        version: request.step.version.to_string(),
        direction: Some(request.step.direction),
        event: match request.step.direction {
            Direction::Up => EventKind::Up,
            Direction::Down => EventKind::Down,
        },
        succeeded,
        checksum: Some(request.step.checksum.clone()),
        started_at: now,
        finished_at: now,
        applied_by: request.identity.applied_by.clone(),
        host: request.identity.host.clone(),
        tool_version: request.identity.tool_version.clone(),
        error,
    }
}

fn applied_row(request: &ExecuteRequest<'_>, status: Status) -> AppliedMigration {
    let step = request.step;
    AppliedMigration {
        version: step.version.to_string(),
        description: step.description.clone(),
        script_name: step.script_name.clone(),
        checksum: step.checksum.clone(),
        kind: step.kind,
        status,
        direction: (status != Status::Applied).then_some(step.direction),
        applied_at: Utc::now(),
        applied_by: request.identity.applied_by.clone(),
        execution_ms: 0,
        tool_version: request.identity.tool_version.clone(),
        executor: "native".into(),
        runner_version: None,
        op_count: None,
        completed_ops: None,
        failed_op: None,
        recovery: None,
        lock_epoch: None,
    }
}

#[async_trait]
impl Provider for MemoryProvider {
    fn kind(&self) -> &'static str {
        "memory"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["sql"]
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            transactional_ddl: true,
            transactional_ledger: true,
            native_lock: true,
        }
    }

    fn prepare(&self, script: &ScriptFile) -> Result<(), String> {
        if self.db.lock().reject.contains(&script.relative_path) {
            return Err(format!("{}: rejected by prepare", script.relative_path));
        }
        Ok(())
    }

    async fn lock_status(&mut self) -> Result<LockStatus, ProviderError> {
        let state = self.db.lock();
        Ok(match &state.lock_holder {
            Some((_, h)) => LockStatus::from_parts(true, Some(h.clone())),
            None => LockStatus::from_parts(false, state.stale_holder.clone()),
        })
    }

    async fn read_ledger(&mut self) -> Result<Vec<AppliedMigration>, ProviderError> {
        Ok(self.db.ledger())
    }

    async fn ensure_ledger(&mut self) -> Result<(), ProviderError> {
        self.db.lock().ledger.get_or_insert_with(Vec::new);
        Ok(())
    }

    async fn acquire_lock(&mut self, options: &LockOptions) -> Result<(), ProviderError> {
        let deadline = Instant::now() + options.timeout;
        loop {
            {
                let mut state = self.db.lock();
                match &state.lock_holder {
                    None => {
                        let holder = LockHolder {
                            owner: options.owner.clone(),
                            principal: "memory".into(),
                            application: "migrate-db/memory".into(),
                            session_id: i64::try_from(self.id).ok(),
                            acquired_at: Utc::now(),
                        };
                        // Acquiring replaces any stale record.
                        state.stale_holder = None;
                        state.lock_holder = Some((self.id, holder));
                        return Ok(());
                    }
                    Some((id, _)) if *id == self.id => return Ok(()),
                    Some((_, holder)) => {
                        if Instant::now() >= deadline {
                            return Err(ProviderError::lock_timeout(Some(&holder.describe())));
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn release_lock(&mut self) -> Result<(), ProviderError> {
        let mut state = self.db.lock();
        if state
            .lock_holder
            .as_ref()
            .is_some_and(|(id, _)| *id == self.id)
        {
            state.lock_holder = None;
        }
        Ok(())
    }

    async fn force_unlock(
        &mut self,
        kill_holder: bool,
        identity: &RunIdentity,
    ) -> Result<UnlockOutcome, ProviderError> {
        let mut state = self.db.lock();
        let outcome = match (state.lock_holder.take(), state.stale_holder.take()) {
            (Some((_, holder)), _) if kill_holder => UnlockOutcome::KilledHolder(holder),
            (Some(held), stale) => {
                let err = ProviderError::lock_held(Some(&held.1));
                state.lock_holder = Some(held);
                state.stale_holder = stale;
                return Err(err);
            }
            (None, Some(stale)) => UnlockOutcome::ClearedStale(Some(stale)),
            (None, None) => UnlockOutcome::NothingToDo,
        };
        if outcome != UnlockOutcome::NothingToDo && state.ledger.is_some() {
            let now = Utc::now();
            state.history.push(HistoryEvent {
                version: String::new(),
                direction: None,
                event: EventKind::ForceUnlock,
                succeeded: true,
                checksum: None,
                started_at: now,
                finished_at: now,
                applied_by: identity.applied_by.clone(),
                host: identity.host.clone(),
                tool_version: identity.tool_version.clone(),
                error: None,
            });
        }
        Ok(outcome)
    }

    async fn execute(
        &mut self,
        request: &ExecuteRequest<'_>,
    ) -> Result<StepOutcome, ProviderError> {
        let step = request.step;
        let key = (step.direction, step.version.to_string());
        let mut guard = self.db.lock();
        let state = &mut *guard; // split borrows across fields
        let ledger = state
            .ledger
            .as_mut()
            .ok_or_else(|| ProviderError::ledger("ledger does not exist"))?;

        if !step.transactional {
            // Mirror real providers: mark the row in progress before the script runs.
            ledger.retain(|r| r.version != key.1);
            ledger.push(applied_row(request, Status::InProgress));
        }

        if request.cancel.is_cancelled() {
            // Interrupted: a transactional step leaves no trace; a
            // non-transactional one keeps its `in_progress` row.
            return Err(ProviderError::new(
                migrate_db_core::provider::ProviderErrorKind::Cancelled,
                format!("{}: cancelled", request.script.relative_path),
            ));
        }

        if state.lose_connection_on.contains(&key) {
            // A non-transactional step keeps its `in_progress` row (fail closed).
            return Err(ProviderError::connection("connection lost (injected)"));
        }

        if state.fail_on.contains(&key) {
            let message = format!("injected failure in {}", request.script.relative_path);
            if !step.transactional {
                // Non-transactional: partial effects may exist → failed row.
                ledger.retain(|r| r.version != key.1);
                ledger.push(applied_row(request, Status::Failed));
            }
            state
                .history
                .push(history(request, false, Some(message.clone())));
            return Err(ProviderError::execution(message));
        }

        ledger.retain(|r| r.version != key.1 || r.status == Status::Applied);
        match step.direction {
            Direction::Up => ledger.push(applied_row(request, Status::Applied)),
            Direction::Down => ledger.retain(|r| r.version != key.1),
        }
        state.history.push(history(request, true, None));
        state.executed.push(key);
        state.scripts.push(request.script.content.clone());
        Ok(StepOutcome { duration_ms: 0 })
    }

    async fn mark(
        &mut self,
        marks: &[PlannedStep],
        identity: &RunIdentity,
    ) -> Result<(), ProviderError> {
        let mut guard = self.db.lock();
        let state = &mut *guard;
        let ledger = state
            .ledger
            .as_mut()
            .ok_or_else(|| ProviderError::ledger("ledger does not exist"))?;
        let now = Utc::now();
        for m in marks {
            ledger.push(AppliedMigration {
                version: m.version.to_string(),
                description: m.description.clone(),
                script_name: m.script_name.clone(),
                checksum: m.checksum.clone(),
                kind: m.kind,
                status: Status::Applied,
                direction: None,
                applied_at: now,
                applied_by: identity.applied_by.clone(),
                execution_ms: 0,
                tool_version: identity.tool_version.clone(),
                executor: "native".into(),
                runner_version: None,
                op_count: None,
                completed_ops: None,
                failed_op: None,
                recovery: None,
                lock_epoch: None,
            });
            state.history.push(HistoryEvent {
                version: m.version.to_string(),
                direction: Some(Direction::Up),
                event: EventKind::Baseline,
                succeeded: true,
                checksum: Some(m.checksum.clone()),
                started_at: now,
                finished_at: now,
                applied_by: identity.applied_by.clone(),
                host: identity.host.clone(),
                tool_version: identity.tool_version.clone(),
                error: None,
            });
        }
        Ok(())
    }
}
