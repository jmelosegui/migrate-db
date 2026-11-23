//! Engine behaviour against the in-memory provider.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;
use std::path::Path;
use std::time::Duration;

use migrate_db_core::engine::{BaselineRequest, Engine, NoopObserver};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::model::{Direction, EventKind, Kind, Status};
use migrate_db_core::planner::{PlanError, Policy, Request};
use migrate_db_core::provider::{LockOptions, Provider, RunIdentity};
use migrate_db_core::source::{DiscoveryOptions, MigrationSet, discover};
use migrate_db_memory::{MemoryDatabase, MemoryProvider};

fn identity() -> RunIdentity {
    RunIdentity {
        applied_by: "tester@host".into(),
        host: "host".into(),
        tool_version: "0.0.0-test".into(),
    }
}

fn lock() -> LockOptions {
    LockOptions {
        timeout: Duration::from_millis(100),
        owner: "test-run".into(),
    }
}

const ALLOW_ALL: Policy = Policy {
    allow_down: true,
    allow_out_of_order: false,
};

struct Fixture {
    _dir: tempfile::TempDir,
    set: MigrationSet,
}

/// `(version, up_header)`; every migration gets a down file.
fn fixture(migrations: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    for (version, header) in migrations {
        write(
            dir.path(),
            &format!("V{version}__m{version}.up.sql"),
            &format!("{header}select {version};\n"),
        );
        write(
            dir.path(),
            &format!("V{version}__m{version}.down.sql"),
            &format!("{header}select -{version};\n"),
        );
    }
    let set = discover(
        dir.path(),
        &DiscoveryOptions {
            extensions: vec!["sql".into()],
            strict_filenames: false,
        },
    )
    .unwrap();
    Fixture { _dir: dir, set }
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

fn up() -> Request {
    Request::Up {
        to: None,
        steps: None,
    }
}

async fn run(
    provider: &mut MemoryProvider,
    set: &MigrationSet,
    policy: Policy,
    request: &Request,
) -> Result<migrate_db_core::engine::RunReport, Error> {
    Engine::new(provider, set, policy, identity())
        .run(request, &lock(), None, &NoopObserver)
        .await
}

fn versions(db: &MemoryDatabase) -> Vec<String> {
    db.ledger().into_iter().map(|r| r.version).collect()
}

#[tokio::test]
async fn read_only_paths_never_create_the_ledger() {
    let f = fixture(&[("1", ""), ("2", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let mut engine = Engine::new(&mut p, &f.set, ALLOW_ALL, identity());

    let status = engine.status().await.unwrap();
    assert_eq!(status.pending(), 2);
    let plan = engine.dry_run(&up()).await.unwrap();
    assert_eq!(plan.steps.len(), 2);

    assert!(!db.ledger_exists(), "status / dry-run must not write");
    assert!(db.executed().is_empty());
    assert_eq!(db.lock_holder(), None, "read-only paths take no lock");
}

#[tokio::test]
async fn up_applies_everything_once_and_releases_the_lock() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();

    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(report.succeeded());
    assert_eq!(report.completed.len(), 3);
    assert_eq!(versions(&db), ["1", "2", "3"]);
    assert_eq!(db.history().len(), 3);
    assert_eq!(db.lock_holder(), None);

    let again = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(again.plan.is_empty());
    assert_eq!(db.executed().len(), 3, "second up is a no-op");
}

#[tokio::test]
async fn transactional_failure_stops_the_run_and_leaves_no_row() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    db.fail_on(Direction::Up, "2");
    let mut p = db.connect();

    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    let failure = report.failure.as_ref().unwrap();
    assert_eq!(failure.version.as_str(), "2");
    assert_eq!(report.completed.len(), 1, "step 1 stays applied");
    assert_eq!(
        versions(&db),
        ["1"],
        "failed transactional step leaves no row"
    );
    assert!(db.history().iter().any(|h| !h.succeeded));
    assert_eq!(db.lock_holder(), None, "lock released after a failure");

    // Re-run after fixing: picks up where it stopped.
    db.clear_failures();
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(report.succeeded());
    assert_eq!(versions(&db), ["1", "2", "3"]);
}

#[tokio::test]
async fn non_transactional_failure_leaves_a_failed_row_that_blocks_runs() {
    let f = fixture(&[("1", ""), ("2", "-- migrate:no-transaction\n")]);
    let db = MemoryDatabase::new();
    db.fail_on(Direction::Up, "2");
    let mut p = db.connect();

    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(!report.succeeded());
    let failed = db.ledger().into_iter().find(|r| r.version == "2").unwrap();
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(failed.direction, Some(Direction::Up));

    db.clear_failures();
    let err = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap_err();
    assert!(matches!(err, Error::Plan(PlanError::Unresolved { .. })));
    assert_eq!(err.exit_code(), exit::VALIDATION);
}

#[tokio::test]
async fn expect_plan_mismatch_executes_nothing() {
    let f = fixture(&[("1", ""), ("2", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let approved = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .dry_run(&up())
        .await
        .unwrap();

    // Someone else migrates between plan and apply.
    let mut other = db.connect();
    run(
        &mut other,
        &f.set,
        ALLOW_ALL,
        &Request::Up {
            to: None,
            steps: Some(1),
        },
    )
    .await
    .unwrap();

    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .run(&up(), &lock(), Some(&approved.fingerprint), &NoopObserver)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PlanMismatch { .. }));
    assert_eq!(versions(&db), ["1"], "nothing beyond the other run");

    // The matching fingerprint is accepted.
    let fresh = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .dry_run(&up())
        .await
        .unwrap();
    let report = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .run(&up(), &lock(), Some(&fresh.fingerprint), &NoopObserver)
        .await
        .unwrap();
    assert!(report.succeeded());
}

#[tokio::test]
async fn second_run_times_out_while_the_lock_is_held() {
    let f = fixture(&[("1", "")]);
    let db = MemoryDatabase::new();
    let mut holder = db.connect();
    holder
        .acquire_lock(&LockOptions {
            timeout: Duration::from_millis(10),
            owner: "pipeline run 42".into(),
        })
        .await
        .unwrap();

    let mut waiter = db.connect();
    let err = run(&mut waiter, &f.set, ALLOW_ALL, &up())
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::LOCK_TIMEOUT);
    assert!(err.to_string().contains("pipeline run 42"), "{err}");
    assert!(!db.ledger_exists(), "nothing happened without the lock");

    holder.release_lock().await.unwrap();
    assert!(
        run(&mut waiter, &f.set, ALLOW_ALL, &up())
            .await
            .unwrap()
            .succeeded()
    );
}

#[tokio::test]
async fn concurrent_runs_apply_each_migration_once() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    let mut a = db.connect();
    let mut b = db.connect();
    let request = up();
    let (ra, rb) = tokio::join!(
        run(&mut a, &f.set, ALLOW_ALL, &request),
        run(&mut b, &f.set, ALLOW_ALL, &request)
    );
    let total = ra.unwrap().completed.len() + rb.unwrap().completed.len();
    assert_eq!(total, 3);
    assert_eq!(versions(&db), ["1", "2", "3"]);
}

#[tokio::test]
async fn down_and_goto_round_trip() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();

    run(
        &mut p,
        &f.set,
        ALLOW_ALL,
        &Request::Down {
            to: None,
            steps: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(versions(&db), ["1", "2"]);

    run(
        &mut p,
        &f.set,
        ALLOW_ALL,
        &Request::Goto {
            target: "0".parse().unwrap(),
        },
    )
    .await
    .unwrap();
    assert!(versions(&db).is_empty());

    run(
        &mut p,
        &f.set,
        ALLOW_ALL,
        &Request::Goto {
            target: "2".parse().unwrap(),
        },
    )
    .await
    .unwrap();
    assert_eq!(versions(&db), ["1", "2"]);

    let err = run(
        &mut p,
        &f.set,
        Policy::default(),
        &Request::Down {
            to: None,
            steps: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Plan(PlanError::DownNotAllowed)));
    assert_eq!(db.lock_holder(), None);
}

#[tokio::test]
async fn edited_applied_migration_is_detected() {
    let f = fixture(&[("1", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    db.edit_ledger(|rows| rows[0].checksum = "something-else".into());

    let status = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .status()
        .await
        .unwrap();
    assert!(status.has_problems());
    let err = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap_err();
    assert!(matches!(err, Error::Plan(PlanError::ChecksumMismatch(_))));
}

#[tokio::test]
async fn a_script_rejected_by_prepare_stops_the_run_before_any_step() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    db.reject_script("V3__m3.up.sql");
    let mut p = db.connect();

    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .dry_run(&up())
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::VALIDATION);
    assert!(err.to_string().contains("V3__m3.up.sql"), "{err}");

    let err = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap_err();
    assert!(matches!(err, Error::InvalidScripts(_)), "{err}");
    assert!(
        db.executed().is_empty(),
        "steps 1 and 2 must not run either"
    );
    assert!(db.lock_holder().is_none(), "lock released");

    // Not in the plan, not checked: `up --to 2` still works.
    let to_2 = Request::Up {
        to: Some("2".parse().unwrap()),
        steps: None,
    };
    assert!(
        run(&mut p, &f.set, ALLOW_ALL, &to_2)
            .await
            .unwrap()
            .succeeded()
    );
}

#[tokio::test]
async fn lock_status_reports_holder_free_and_stale_records() {
    let db = MemoryDatabase::new();
    let mut observer = db.connect();
    let free = observer.lock_status().await.unwrap();
    assert!(!free.held && free.holder.is_none() && !free.stale_info);

    let mut holder = db.connect();
    holder.acquire_lock(&lock()).await.unwrap();
    let held = observer.lock_status().await.unwrap();
    assert!(held.held && !held.stale_info);
    assert_eq!(held.holder.as_ref().unwrap().owner.os_user, "test-run");

    let record = held.holder.unwrap();
    holder.release_lock().await.unwrap();
    db.leave_stale_holder(record.clone());
    let stale = observer.lock_status().await.unwrap();
    assert!(!stale.held && stale.stale_info);
    assert_eq!(stale.holder, Some(record));

    // The next acquirer replaces the stale record.
    holder.acquire_lock(&lock()).await.unwrap();
    holder.release_lock().await.unwrap();
    assert!(!observer.lock_status().await.unwrap().stale_info);
}

/// Two migrations that use placeholders, in a temp dir.
fn placeholder_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "V1__a.up.sql",
        "create table ${schema}.a (id int);\n",
    );
    write(
        dir.path(),
        "V2__b.up.sql",
        "grant select on ${schema}.a to ${reader};\n",
    );
    let set = discover(
        dir.path(),
        &DiscoveryOptions {
            extensions: vec!["sql".into()],
            strict_filenames: false,
        },
    )
    .unwrap();
    Fixture { _dir: dir, set }
}

#[tokio::test]
async fn placeholders_are_substituted_but_checksums_stay_on_the_raw_file() {
    use migrate_db_core::placeholders::Values;
    let f = placeholder_fixture();
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let values = Values::from_vars([
        ("MIGRATE_DB_PLACEHOLDER_SCHEMA", "sales"),
        ("MIGRATE_DB_PLACEHOLDER_READER", "report_role"),
    ]);
    let report = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .with_placeholders(Some(values))
        .run(&up(), &lock(), None, &NoopObserver)
        .await
        .unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    assert_eq!(
        db.executed_scripts(),
        [
            "create table sales.a (id int);\n",
            "grant select on sales.a to report_role;\n"
        ]
    );
    let raw = f.set.versioned[0].up.checksum.to_string();
    assert_eq!(db.ledger()[0].checksum, raw, "checksum of the raw file");
}

#[tokio::test]
async fn a_missing_placeholder_value_stops_the_run_before_any_step() {
    use migrate_db_core::placeholders::Values;
    let f = placeholder_fixture();
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let only_schema = Values::from_vars([("MIGRATE_DB_PLACEHOLDER_SCHEMA", "sales")]);
    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .with_placeholders(Some(only_schema.clone()))
        .dry_run(&up())
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::VALIDATION);
    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .with_placeholders(Some(only_schema))
        .run(&up(), &lock(), None, &NoopObserver)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("V2__b.up.sql") && text.contains("MIGRATE_DB_PLACEHOLDER_READER"),
        "{text}"
    );
    assert!(db.executed().is_empty(), "step 1 didn't run either");

    // Disabled: scripts run exactly as written.
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(report.succeeded());
    assert!(db.executed_scripts()[0].contains("${schema}"));
}

#[tokio::test]
async fn a_lost_connection_mid_run_reports_what_completed_with_exit_5() {
    use migrate_db_core::engine::FailureKind;
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    db.lose_connection_on(Direction::Up, "2");
    let mut p = db.connect();
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    let ids: Vec<String> = report
        .completed
        .iter()
        .map(|s| s.version.to_string())
        .collect();
    assert_eq!(ids, ["1"], "step 1 committed and is reported");
    let failure = report.failure.as_ref().unwrap();
    assert_eq!(
        (failure.version.as_str(), failure.kind),
        ("2", FailureKind::Connection)
    );
    assert!(
        failure.error.contains("outcome is unknown"),
        "{}",
        failure.error
    );
    assert_eq!(report.exit_code(), exit::CONNECTION);
    assert!(db.lock_holder().is_none(), "lock released");

    // A script error keeps exit 4.
    db.clear_failures();
    db.fail_on(Direction::Up, "2");
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert_eq!(report.exit_code(), exit::MIGRATION_FAILED);
    db.clear_failures();
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert_eq!(report.exit_code(), exit::OK);
}

#[tokio::test]
async fn a_lost_connection_in_a_non_transactional_step_fails_closed() {
    let f = fixture(&[("1", "-- migrate:no-transaction\n")]);
    let db = MemoryDatabase::new();
    db.lose_connection_on(Direction::Up, "1");
    let mut p = db.connect();
    let report = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(!report.succeeded());
    assert_eq!(
        db.ledger()[0].status,
        Status::InProgress,
        "blocks the next run"
    );
    db.clear_failures();
    let err = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::Unresolved { .. })),
        "{err}"
    );
}

/// Cancels the token once a step has finished, like Ctrl-C between steps.
struct CancelAfterFirstStep(migrate_db_core::cancel::CancelToken);

impl migrate_db_core::engine::Observer for CancelAfterFirstStep {
    fn step_finished(
        &self,
        _step: &migrate_db_core::planner::PlannedStep,
        _result: &Result<
            migrate_db_core::provider::StepOutcome,
            migrate_db_core::provider::ProviderError,
        >,
    ) {
        self.0.cancel();
    }
}

#[tokio::test]
async fn cancelling_between_steps_stops_the_run_with_exit_4() {
    use migrate_db_core::cancel::CancelToken;
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let token = CancelToken::new();
    let report = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .with_cancel(token.clone())
        .run(&up(), &lock(), None, &CancelAfterFirstStep(token))
        .await
        .unwrap();
    assert!(report.cancelled && report.failure.is_none());
    let ids: Vec<String> = report
        .completed
        .iter()
        .map(|s| s.version.to_string())
        .collect();
    assert_eq!(ids, ["1"]);
    assert_eq!(report.exit_code(), exit::MIGRATION_FAILED);
    assert_eq!(db.executed().len(), 1);
    assert!(db.lock_holder().is_none(), "lock released");
}

#[tokio::test]
async fn cancelling_before_or_while_waiting_for_the_lock_runs_nothing() {
    use migrate_db_core::cancel::CancelToken;
    let f = fixture(&[("1", "")]);
    let db = MemoryDatabase::new();

    let token = CancelToken::new();
    token.cancel();
    let mut p = db.connect();
    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .with_cancel(token)
        .run(&up(), &lock(), None, &NoopObserver)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled));
    assert_eq!(err.exit_code(), exit::MIGRATION_FAILED);

    let mut holder = db.connect();
    holder.acquire_lock(&lock()).await.unwrap();
    let token = CancelToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        canceller.cancel();
    });
    let started = std::time::Instant::now();
    let mut waiter = db.connect();
    let long_wait = LockOptions {
        timeout: Duration::from_secs(30),
        owner: "waiter".into(),
    };
    let err = Engine::new(&mut waiter, &f.set, ALLOW_ALL, identity())
        .with_cancel(token)
        .run(&up(), &long_wait, None, &NoopObserver)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "didn't wait out the lock"
    );
    assert!(db.executed().is_empty());
}

/// Records lock-wait reports.
#[derive(Default)]
struct WaitLog(std::sync::Mutex<Vec<(Duration, Option<String>)>>);

impl migrate_db_core::engine::Observer for WaitLog {
    fn lock_waiting(&self, waited: Duration, lock: Option<&migrate_db_core::provider::LockStatus>) {
        let who = lock
            .and_then(|l| l.holder.as_ref())
            .map(|h| h.owner.os_user.clone());
        self.0.lock().unwrap().push((waited, who));
    }
}

#[tokio::test]
async fn waiting_for_the_lock_reports_the_holder_periodically() {
    let f = fixture(&[("1", "")]);
    let db = MemoryDatabase::new();
    let mut holder = db.connect();
    holder
        .acquire_lock(&LockOptions {
            timeout: Duration::from_millis(10),
            owner: "pipeline run 42".into(),
        })
        .await
        .unwrap();

    let mut waiter = db.connect();
    let log = WaitLog::default();
    let err = Engine::new(&mut waiter, &f.set, ALLOW_ALL, identity())
        .with_lock_report_interval(Duration::from_millis(30))
        .run(
            &up(),
            &LockOptions {
                timeout: Duration::from_millis(200),
                owner: "waiter".into(),
            },
            None,
            &log,
        )
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::LOCK_TIMEOUT);
    assert!(err.to_string().contains("pipeline run 42"), "{err}");
    let reports = log.0.lock().unwrap().clone();
    assert!(reports.len() >= 3, "{reports:?}");
    assert!(
        reports
            .iter()
            .all(|(_, who)| who.as_deref() == Some("pipeline run 42"))
    );
    assert!(reports.windows(2).all(|w| w[0].0 < w[1].0), "waited grows");

    // The lock frees up during the wait: the run proceeds.
    let log = WaitLog::default();
    let release = async {
        tokio::time::sleep(Duration::from_millis(80)).await;
        holder.release_lock().await.unwrap();
    };
    let mut engine = Engine::new(&mut waiter, &f.set, ALLOW_ALL, identity())
        .with_lock_report_interval(Duration::from_millis(30));
    let request = up();
    let long_wait = LockOptions {
        timeout: Duration::from_secs(5),
        owner: "waiter".into(),
    };
    let run = engine.run(&request, &long_wait, None, &log);
    let ((), report) = tokio::join!(release, run);
    assert!(report.unwrap().succeeded());
    assert!(!log.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn force_unlock_clears_stale_records_and_kills_only_when_asked() {
    use migrate_db_core::provider::UnlockOutcome;
    let f = fixture(&[("1", "")]);
    let db = MemoryDatabase::new();
    let mut admin = db.connect();

    // Free, no record.
    let mut engine = Engine::new(&mut admin, &f.set, ALLOW_ALL, identity());
    assert_eq!(
        engine.force_unlock(false).await.unwrap(),
        UnlockOutcome::NothingToDo
    );

    // Held: refused without --kill-holder (exit 3), the lock stays.
    let mut holder = db.connect();
    holder.acquire_lock(&lock()).await.unwrap();
    let err = Engine::new(&mut admin, &f.set, ALLOW_ALL, identity())
        .force_unlock(false)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::LOCK_TIMEOUT);
    assert!(err.to_string().contains("--kill-holder"), "{err}");
    assert!(db.lock_holder().is_some());

    // Held, --kill-holder: the holder is gone and the next run proceeds.
    let killed = Engine::new(&mut admin, &f.set, ALLOW_ALL, identity())
        .force_unlock(true)
        .await
        .unwrap();
    assert!(matches!(killed, UnlockOutcome::KilledHolder(h) if h.owner.os_user == "test-run"));
    assert!(db.lock_holder().is_none());
    assert!(
        run(&mut admin, &f.set, ALLOW_ALL, &up())
            .await
            .unwrap()
            .succeeded()
    );

    // Stale record: cleared and audited.
    let record = {
        holder.acquire_lock(&lock()).await.unwrap();
        let h = db.lock_holder().unwrap();
        holder.release_lock().await.unwrap();
        h
    };
    db.leave_stale_holder(record.clone());
    let cleared = Engine::new(&mut admin, &f.set, ALLOW_ALL, identity())
        .force_unlock(false)
        .await
        .unwrap();
    assert_eq!(cleared, UnlockOutcome::ClearedStale(Some(record)));
    assert!(!admin.lock_status().await.unwrap().stale_info);
    let unlocks = db
        .history()
        .into_iter()
        .filter(|e| e.event == migrate_db_core::model::EventKind::ForceUnlock)
        .count();
    // The kill came before the first run created the ledger, and unlock
    // never creates storage, so only the clear is in the history log.
    assert_eq!(unlocks, 1);
}

fn baseline(version: &migrate_db_core::version::Version) -> BaselineRequest<'_> {
    BaselineRequest {
        version,
        description: Some("baseline"),
        mark_repeatables: false,
    }
}

#[tokio::test]
async fn baseline_mark_writes_the_marker_and_executes_nothing() {
    let f = fixture(&[("1", ""), ("2", ""), ("3", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let v2 = "2".parse().unwrap();

    // Dry run: read-only, as for `up`.
    let mut engine = Engine::new(&mut p, &f.set, ALLOW_ALL, identity());
    let preview = engine.dry_run_baseline(&baseline(&v2)).await.unwrap();
    assert_eq!(preview.steps.len(), 1);
    assert!(!db.ledger_exists(), "dry run must not write");
    assert_eq!(db.lock_holder(), None);

    let report = engine
        .mark_baseline(
            &baseline(&v2),
            &lock(),
            Some(&preview.fingerprint),
            &NoopObserver,
        )
        .await
        .unwrap();
    assert_eq!(report.plan.fingerprint, preview.fingerprint);
    assert!(
        db.executed().is_empty(),
        "baseline mark never runs a script"
    );
    let rows = db.ledger();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].version, "2");
    assert_eq!(rows[0].kind, Kind::Baseline);
    assert_eq!(rows[0].description, "baseline");
    let history = db.history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].event, EventKind::Baseline);
    assert_eq!(db.lock_holder(), None, "lock released");

    // Versions 1 and 2 count as applied; `up` runs only 3.
    let next = run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    assert!(next.succeeded());
    assert_eq!(db.executed(), [(Direction::Up, "3".to_owned())]);
}

#[tokio::test]
async fn baseline_mark_refuses_a_ledger_in_use_and_a_wrong_plan() {
    let f = fixture(&[("1", ""), ("2", "")]);
    let db = MemoryDatabase::new();
    let mut p = db.connect();
    let v1 = "1".parse().unwrap();

    // A plan approved for something else writes nothing.
    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .mark_baseline(
            &baseline(&v1),
            &lock(),
            Some("not-this-plan"),
            &NoopObserver,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::PlanMismatch { .. }), "{err:?}");
    assert!(db.ledger().is_empty());

    // Once migrate-db manages the database it is never re-baselined.
    run(&mut p, &f.set, ALLOW_ALL, &up()).await.unwrap();
    let err = Engine::new(&mut p, &f.set, ALLOW_ALL, identity())
        .mark_baseline(&baseline(&v1), &lock(), None, &NoopObserver)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::LedgerNotEmpty(2))),
        "{err:?}"
    );
    assert_eq!(err.exit_code(), exit::VALIDATION);
    assert_eq!(db.lock_holder(), None, "lock released after a refusal");
}
