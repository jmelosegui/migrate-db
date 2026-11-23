//! The contract cases. Each takes a [`Harness`] and panics on a violation.

use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::engine::{
    BaselineRequest, Engine, FailureKind, NoopObserver, Observer, RunReport,
};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::model::{Kind, Status};
use migrate_db_core::planner::{MigrationState, PlanError, Policy, Request};
use migrate_db_core::provider::{
    LockOptions, LockStatus, Provider, ProviderErrorKind, RunIdentity, UnlockOutcome,
};
use migrate_db_core::source::{DiscoveryOptions, MigrationSet, discover};

use crate::{Harness, Target};

const POLICY: Policy = Policy {
    allow_down: true,
    allow_out_of_order: false,
};

fn identity() -> RunIdentity {
    RunIdentity {
        applied_by: "conformance@host".into(),
        host: "host".into(),
        tool_version: "0.0.0-conformance".into(),
    }
}

fn lock(ms: u64, owner: &str) -> LockOptions {
    LockOptions {
        timeout: Duration::from_millis(ms),
        owner: owner.into(),
    }
}

fn up() -> Request {
    Request::Up {
        to: None,
        steps: None,
    }
}

fn down_to_zero() -> Request {
    Request::Down {
        to: Some("0".parse().unwrap()),
        steps: None,
    }
}

/// Migration files in a temp folder.
struct Files {
    dir: tempfile::TempDir,
}

impl Files {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// `V<version>__m<version>.up.sql` (and `.down.sql` when given).
    fn versioned(self, version: &str, up: &str, down: Option<&str>) -> Self {
        let root = self.dir.path();
        fs::write(root.join(format!("V{version}__m{version}.up.sql")), up).unwrap();
        if let Some(down) = down {
            fs::write(root.join(format!("V{version}__m{version}.down.sql")), down).unwrap();
        }
        self
    }

    /// `R__<name>.sql`.
    fn repeatable(self, name: &str, body: &str) -> Self {
        fs::write(self.dir.path().join(format!("R__{name}.sql")), body).unwrap();
        self
    }

    fn set(&self) -> MigrationSet {
        discover(
            self.dir.path(),
            &DiscoveryOptions {
                extensions: vec!["sql".into()],
                strict_filenames: false,
            },
        )
        .unwrap()
    }
}

async fn run(p: &mut dyn Provider, set: &MigrationSet, request: &Request) -> RunReport {
    Engine::new(p, set, POLICY, identity())
        .run(request, &lock(5_000, "conformance"), None, &NoopObserver)
        .await
        .unwrap()
}

/// A first successful run, so the ledger (and the meta table that holds the
/// holder record) exists.
async fn bootstrap(h: &impl Harness, t: &Target) -> (Box<dyn Provider>, Files) {
    let files = Files::new().versioned("1", &h.create_table(t, "boot"), None);
    let mut p = h.connect(t, "migrate-db/conformance").await;
    assert!(run(p.as_mut(), &files.set(), &up()).await.succeeded());
    (p, files)
}

/// SPEC §7 rule 8 / AGENTS rule 4: reading never writes.
pub async fn read_only_operations_create_nothing(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new().versioned("1", &h.create_table(&t, "a"), None);
    let set = files.set();
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    assert!(p.read_ledger().await.unwrap().is_empty());
    let lock = p.lock_status().await.unwrap();
    assert!(
        !lock.held && lock.holder.is_none() && !lock.stale_info,
        "{lock:?}"
    );
    let mut engine = Engine::new(p.as_mut(), &set, POLICY, identity());
    assert_eq!(engine.status().await.unwrap().pending(), 1);
    assert_eq!(engine.dry_run(&up()).await.unwrap().steps.len(), 1);
    assert!(
        !h.ledger_storage_exists(&t).await,
        "status, dry-run, read_ledger and lock_status must not create storage"
    );
    assert!(!h.table_exists(&t, "a").await);
}

/// `ensure_ledger` is idempotent and creates an empty ledger.
pub async fn ensure_ledger_is_idempotent(h: &impl Harness) {
    let t = h.new_target();
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    p.ensure_ledger().await.unwrap();
    p.ensure_ledger().await.unwrap();
    assert!(h.ledger_storage_exists(&t).await);
    assert!(p.read_ledger().await.unwrap().is_empty());
}

/// Up applies scripts and rows exactly once; down reverts both; every step
/// is in the history log (SPEC §6.1–6.3).
pub async fn up_and_down_record_ledger_and_history(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new()
        .versioned("1", &h.create_table(&t, "a"), Some(&h.drop_table(&t, "a")))
        .versioned("2", &h.create_table(&t, "b"), Some(&h.drop_table(&t, "b")));
    let set = files.set();
    let mut p = h.connect(&t, "migrate-db/conformance").await;

    let report = run(p.as_mut(), &set, &up()).await;
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(h.table_exists(&t, "a").await && h.table_exists(&t, "b").await);
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 2);
    for (row, m) in ledger.iter().zip(&set.versioned) {
        assert_eq!(row.version, m.version.to_string());
        assert_eq!(
            row.checksum,
            m.up.checksum.to_string(),
            "checksum round-trips"
        );
        assert_eq!(row.script_name, m.up.file_name);
        assert_eq!((row.kind, row.status), (Kind::Versioned, Status::Applied));
        assert_eq!(row.description, m.description);
    }

    assert!(
        run(p.as_mut(), &set, &up()).await.plan.is_empty(),
        "nothing to do twice"
    );

    let report = run(p.as_mut(), &set, &down_to_zero()).await;
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(!h.table_exists(&t, "a").await && !h.table_exists(&t, "b").await);
    assert!(p.read_ledger().await.unwrap().is_empty());
    let events: Vec<(String, String, String)> = h
        .history(&t)
        .await
        .into_iter()
        .map(|r| (r.version, r.event, r.status))
        .collect();
    let ok = |v: &str, e: &str| (v.to_owned(), e.to_owned(), "succeeded".to_owned());
    assert_eq!(
        events,
        [
            ok("1", "up"),
            ok("2", "up"),
            ok("2", "down"),
            ok("1", "down")
        ]
    );
}

/// A failing transactional step leaves no trace but a failed history row,
/// and earlier steps stay applied (SPEC §6.3).
pub async fn failing_transactional_step_rolls_back_completely(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new()
        .versioned("1", &h.create_table(&t, "a"), None)
        .versioned("2", &h.create_then_fail(&t, "b"), None);
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    let report = run(p.as_mut(), &files.set(), &up()).await;
    let failure = report.failure.as_ref().expect("step 2 must fail");
    assert_eq!(failure.version.as_str(), "2");
    assert_eq!(failure.kind, FailureKind::Execution, "{}", failure.error);
    assert_eq!(report.exit_code(), exit::MIGRATION_FAILED);
    assert_eq!(report.completed.len(), 1, "step 1 is reported as completed");
    assert!(h.table_exists(&t, "a").await, "step 1 stays");
    assert!(!h.table_exists(&t, "b").await, "step 2 rolled back");
    assert_eq!(p.read_ledger().await.unwrap().len(), 1);
    let failed: Vec<_> = h
        .history(&t)
        .await
        .into_iter()
        .filter(|r| r.status == "failed")
        .collect();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        (failed[0].version.as_str(), failed[0].event.as_str()),
        ("2", "up")
    );
    let lock = p.lock_status().await.unwrap();
    assert!(
        !lock.held && lock.holder.is_none(),
        "lock released: {lock:?}"
    );
}

/// A failing non-transactional step leaves a `failed` row that blocks later
/// runs until it is resolved (SPEC §6.3).
pub async fn failing_non_transactional_step_leaves_a_blocking_failed_row(h: &impl Harness) {
    let t = h.new_target();
    let script = format!("-- migrate:no-transaction\n{}", h.create_then_fail(&t, "a"));
    let files = Files::new().versioned("1", &script, None);
    let set = files.set();
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    let report = run(p.as_mut(), &set, &up()).await;
    assert!(report.failure.is_some());
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].status, Status::Failed);
    let err = Engine::new(p.as_mut(), &set, POLICY, identity())
        .run(&up(), &lock(5_000, "conformance"), None, &NoopObserver)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::Unresolved { .. })),
        "{err}"
    );
    assert_eq!(err.exit_code(), exit::VALIDATION);
}

/// One holder at a time; a timeout names the holder (exit 3); the lock dies
/// with the holder's session.
pub async fn lock_is_exclusive_and_dies_with_its_session(h: &impl Harness) {
    let t = h.new_target();
    let (_p, _files) = bootstrap(h, &t).await;
    let mut holder = h.connect(&t, "migrate-db/conformance-holder").await;
    holder
        .acquire_lock(&lock(2_000, "run-42@agent"))
        .await
        .unwrap();

    let mut other = h.connect(&t, "migrate-db/conformance").await;
    let started = Instant::now();
    let err = other.acquire_lock(&lock(300, "other")).await.unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::LockTimeout);
    assert!(started.elapsed() >= Duration::from_millis(250), "it waited");
    assert!(err.message.contains("run-42@agent"), "{}", err.message);
    assert_eq!(Error::from(err).exit_code(), exit::LOCK_TIMEOUT);

    drop(holder); // a crashed run: no release
    other.acquire_lock(&lock(5_000, "other")).await.unwrap();
    other.release_lock().await.unwrap();
    let after = other.lock_status().await.unwrap();
    assert!(!after.held && after.holder.is_none(), "{after:?}");
}

/// The holder record while held, removed on release, stale after a crash,
/// replaced by the next holder (SPEC §7 rule 10).
pub async fn holder_record_is_visible_and_stale_after_a_crash(h: &impl Harness) {
    let t = h.new_target();
    let (mut observer, files) = bootstrap(h, &t).await;
    let mut holder = h.connect(&t, "migrate-db/conformance-holder").await;
    holder
        .acquire_lock(&lock(2_000, "builder@agent-07"))
        .await
        .unwrap();

    let held = observer.lock_status().await.unwrap();
    assert!(held.held && !held.stale_info, "{held:?}");
    let record = held.holder.expect("holder record while held");
    assert_eq!(record.owner.os_user, "builder@agent-07");
    assert_eq!(record.application, "migrate-db/conformance-holder");
    assert!(record.session_id.is_some(), "session id for --kill-holder");
    assert!(!record.principal.is_empty());

    holder.release_lock().await.unwrap();
    let free = observer.lock_status().await.unwrap();
    assert!(
        !free.held && free.holder.is_none(),
        "record removed: {free:?}"
    );

    let mut crashed = h.connect(&t, "migrate-db/conformance-crashed").await;
    crashed.acquire_lock(&lock(2_000, "crashed")).await.unwrap();
    drop(crashed);
    let stale = wait_for(&mut observer, |s| !s.held).await;
    assert!(stale.stale_info, "{stale:?}");
    assert_eq!(stale.holder.unwrap().owner.os_user, "crashed");

    assert!(
        run(observer.as_mut(), &files.set(), &up())
            .await
            .succeeded()
    );
    let after = observer.lock_status().await.unwrap();
    assert!(!after.stale_info && after.holder.is_none(), "{after:?}");
}

/// Poll `lock_status` until `done` (a dropped session can take a moment
/// to end on the server).
async fn wait_for(p: &mut Box<dyn Provider>, done: impl Fn(&LockStatus) -> bool) -> LockStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let s = p.lock_status().await.unwrap();
        if done(&s) || Instant::now() > deadline {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[derive(Default)]
struct WaitLog(Mutex<Vec<Option<String>>>);

impl Observer for WaitLog {
    fn lock_waiting(&self, _waited: Duration, lock: Option<&LockStatus>) {
        let who = lock
            .and_then(|l| l.holder.as_ref())
            .map(|h| h.owner.os_user.clone());
        self.0.lock().unwrap().push(who);
    }
}

/// A waiting run reports the holder periodically (SPEC §7 rule 10).
pub async fn waiting_run_reports_the_holder(h: &impl Harness) {
    let t = h.new_target();
    let (mut waiter, files) = bootstrap(h, &t).await;
    let mut holder = h.connect(&t, "migrate-db/conformance-holder").await;
    holder
        .acquire_lock(&lock(2_000, "pipeline run 42"))
        .await
        .unwrap();
    let log = WaitLog::default();
    let set = files.set();
    let err = Engine::new(waiter.as_mut(), &set, POLICY, identity())
        .with_lock_report_interval(Duration::from_millis(200))
        .run(&up(), &lock(900, "waiter"), None, &log)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::LOCK_TIMEOUT);
    let reports = log.0.lock().unwrap().clone();
    assert!(reports.len() >= 2, "{reports:?}");
    assert!(
        reports
            .iter()
            .all(|w| w.as_deref() == Some("pipeline run 42")),
        "{reports:?}"
    );
    holder.release_lock().await.unwrap();
}

/// `unlock --force [--kill-holder]` (SPEC §4.1): clears only stale
/// records, refuses a held lock without kill, never kills an unverified
/// session, kills the real holder, and audits what it did.
pub async fn force_unlock_follows_the_contract(h: &impl Harness) {
    let t = h.new_target();
    let (mut admin, _files) = bootstrap(h, &t).await;
    let who = identity();
    let unlocks = |rows: Vec<crate::HistoryRow>| {
        rows.into_iter()
            .filter(|r| r.event == "force-unlock")
            .count()
    };

    assert_eq!(
        admin.force_unlock(false, &who).await.unwrap(),
        UnlockOutcome::NothingToDo
    );
    assert_eq!(
        unlocks(h.history(&t).await),
        0,
        "nothing done, nothing audited"
    );

    let mut crashed = h.connect(&t, "migrate-db/conformance-crashed").await;
    crashed.acquire_lock(&lock(2_000, "crashed")).await.unwrap();
    drop(crashed);
    assert!(wait_for(&mut admin, |s| !s.held).await.stale_info);
    let cleared = admin.force_unlock(false, &who).await.unwrap();
    assert!(
        matches!(&cleared, UnlockOutcome::ClearedStale(Some(r)) if r.owner.os_user == "crashed"),
        "{cleared:?}"
    );
    assert!(admin.lock_status().await.unwrap().holder.is_none());
    assert_eq!(unlocks(h.history(&t).await), 1);

    let mut holder = h.connect(&t, "migrate-db/conformance-holder").await;
    holder.acquire_lock(&lock(2_000, "holder")).await.unwrap();
    let err = admin.force_unlock(false, &who).await.unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::LockHeld, "{}", err.message);
    assert!(err.message.contains("--kill-holder"), "{}", err.message);
    assert_eq!(Error::from(err).exit_code(), exit::LOCK_TIMEOUT);

    // A record naming a session that doesn't hold the lock: kill nothing.
    h.point_holder_at_another_session(&t).await;
    let err = admin.force_unlock(true, &who).await.unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::LockHeld, "{}", err.message);
    assert!(
        err.message.contains("nothing was killed"),
        "{}",
        err.message
    );
    assert!(admin.lock_status().await.unwrap().held, "holder untouched");
    holder.release_lock().await.unwrap();

    let mut holder = h.connect(&t, "migrate-db/conformance-holder").await;
    holder.acquire_lock(&lock(2_000, "holder")).await.unwrap();
    let killed = admin.force_unlock(true, &who).await.unwrap();
    assert!(
        matches!(&killed, UnlockOutcome::KilledHolder(r) if r.owner.os_user == "holder"),
        "{killed:?}"
    );
    let after = wait_for(&mut admin, |s| !s.held).await;
    assert!(!after.held && after.holder.is_none(), "{after:?}");
    assert_eq!(unlocks(h.history(&t).await), 2);
    let mut next = h.connect(&t, "migrate-db/conformance").await;
    next.acquire_lock(&lock(2_000, "next")).await.unwrap();
    next.release_lock().await.unwrap();
}

/// Repeatables run after versioned steps, only when new or changed
/// (SPEC §3.6).
pub async fn repeatables_reapply_only_when_changed(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new()
        .versioned("1", &h.create_table(&t, "a"), None)
        .repeatable("v_marker", &h.define_view(&t, "v_marker", 1));
    let mut p = h.connect(&t, "migrate-db/conformance").await;

    let report = run(p.as_mut(), &files.set(), &up()).await;
    assert!(report.succeeded(), "{:?}", report.failure);
    let order: Vec<String> = report
        .completed
        .iter()
        .map(|s| s.version.to_string())
        .collect();
    assert_eq!(order, ["1", "R__v_marker"], "versioned first");
    assert!(h.view_returns(&t, "v_marker", 1).await);
    let row = p
        .read_ledger()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == Kind::Repeatable)
        .expect("repeatable row");
    assert_eq!(row.version, "R__v_marker");

    assert!(
        run(p.as_mut(), &files.set(), &up()).await.plan.is_empty(),
        "unchanged"
    );

    let files = files.repeatable("v_marker", &h.define_view(&t, "v_marker", 2));
    let set = files.set();
    let status = Engine::new(p.as_mut(), &set, POLICY, identity())
        .status()
        .await
        .unwrap();
    let entry = status
        .entries
        .iter()
        .find(|e| e.version == "R__v_marker")
        .unwrap();
    assert_eq!(entry.state, MigrationState::Changed);
    let report = run(p.as_mut(), &set, &up()).await;
    assert_eq!(report.completed.len(), 1, "only the changed repeatable");
    assert!(h.view_returns(&t, "v_marker", 2).await);
    let reapplied = p
        .read_ledger()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.kind == Kind::Repeatable)
        .unwrap();
    assert_eq!(
        reapplied.checksum,
        set.repeatables[0].script.checksum.to_string()
    );
}

/// Ctrl-C / SIGTERM interrupts the running statement; the step rolls back
/// and the lock is released (SPEC §7 rule 7).
pub async fn cancel_interrupts_the_running_step_and_rolls_back(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new().versioned("1", &h.create_then_sleep(&t, "c", 30), None);
    let set = files.set();
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    let token = CancelToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        canceller.cancel();
    });
    let started = Instant::now();
    let report = Engine::new(p.as_mut(), &set, POLICY, identity())
        .with_cancel(token)
        .run(&up(), &lock(5_000, "conformance"), None, &NoopObserver)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "interrupted, not waited out"
    );
    let failure = report.failure.as_ref().expect("cancelled step");
    assert_eq!(failure.kind, FailureKind::Cancelled, "{}", failure.error);
    assert_eq!(report.exit_code(), exit::MIGRATION_FAILED);
    assert!(!h.table_exists(&t, "c").await, "rolled back");
    assert!(
        p.read_ledger().await.unwrap().is_empty(),
        "nothing recorded"
    );
    let mut next = h.connect(&t, "migrate-db/conformance").await;
    next.acquire_lock(&lock(2_000, "next")).await.unwrap();
    next.release_lock().await.unwrap();
}

/// `baseline mark` writes the marker (and, with `--mark-repeatables`, the
/// repeatables) in the ledger and history, executes no script, and refuses
/// a ledger that is already in use (SPEC §4.1, ADOPTION.md step 6).
pub async fn baseline_mark_writes_rows_and_executes_nothing(h: &impl Harness) {
    let t = h.new_target();
    let files = Files::new()
        .versioned("1", &h.create_table(&t, "a"), None)
        .versioned("2", &h.create_table(&t, "b"), None)
        .repeatable("v", &h.define_view(&t, "v", 1));
    let set = files.set();
    let mut p = h.connect(&t, "migrate-db/conformance").await;
    let version = "2".parse().unwrap();
    let request = BaselineRequest {
        version: &version,
        description: Some("baseline"),
        mark_repeatables: true,
    };

    let report = Engine::new(p.as_mut(), &set, POLICY, identity())
        .mark_baseline(&request, &lock(5_000, "conformance"), None, &NoopObserver)
        .await
        .unwrap();
    assert_eq!(report.plan.steps.len(), 2);
    assert!(
        !h.table_exists(&t, "a").await && !h.table_exists(&t, "b").await,
        "baseline mark must not execute any script"
    );
    let ledger = p.read_ledger().await.unwrap();
    let rows: Vec<(String, Kind, Status, String)> = ledger
        .iter()
        .map(|r| (r.version.clone(), r.kind, r.status, r.checksum.clone()))
        .collect();
    let v2 = set.get(&version).unwrap();
    assert!(rows.contains(&(
        "2".into(),
        Kind::Baseline,
        Status::Applied,
        v2.up.checksum.to_string()
    )));
    assert!(
        rows.iter()
            .any(|r| r.0 == "R__v" && r.1 == Kind::Repeatable && r.2 == Status::Applied),
        "{rows:?}"
    );
    let events: Vec<(String, String)> = h
        .history(&t)
        .await
        .into_iter()
        .map(|r| (r.event, r.status))
        .collect();
    assert_eq!(
        events,
        [
            ("baseline".to_owned(), "succeeded".to_owned()),
            ("baseline".to_owned(), "succeeded".to_owned())
        ]
    );

    // Everything is covered now: `up` has nothing to do.
    assert!(run(p.as_mut(), &set, &up()).await.plan.is_empty());
    let status = Engine::new(p.as_mut(), &set, POLICY, identity())
        .status()
        .await
        .unwrap();
    assert!(
        status
            .entries
            .iter()
            .any(|e| e.version == "2" && e.state == MigrationState::Baseline),
        "status shows the baseline"
    );

    // A second mark is refused and writes nothing.
    let err = Engine::new(p.as_mut(), &set, POLICY, identity())
        .mark_baseline(&request, &lock(5_000, "conformance"), None, &NoopObserver)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::LedgerNotEmpty(_))),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), exit::VALIDATION);
    assert_eq!(p.read_ledger().await.unwrap().len(), 2);
}
