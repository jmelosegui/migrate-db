//! Integration tests against a real SQL Server.
//!
//! Run with a disposable container (PowerShell):
//! ```text
//! podman run -d --name migrate-db-mssql -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD=Migrate-test-1 `
//!   -p 51433:1433 mcr.microsoft.com/mssql/server:2022-latest
//! podman exec -e SQLCMDPASSWORD=Migrate-test-1 migrate-db-mssql /opt/mssql-tools18/bin/sqlcmd `
//!   -C -S localhost -U sa -b -Q "IF DB_ID('migrate_test') IS NULL CREATE DATABASE migrate_test"
//! $env:MIGRATE_DB_TEST_MSSQL = "Server=tcp:localhost,51433;Database=migrate_test;User ID=sa;Password=Migrate-test-1;TrustServerCertificate=true"
//! cargo test -p migrate-db-mssql --features integration
//! ```
//! Every test uses its own schema (and so its own lock), so tests can run in parallel.
#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::fs;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use migrate_db_core::engine::{Engine, NoopObserver, RunReport};
use migrate_db_core::error::{Error, exit};
use migrate_db_core::model::{Direction, Status};
use migrate_db_core::planner::{PlanError, Policy, Request};
use migrate_db_core::provider::{LockOptions, Provider, RunIdentity};
use migrate_db_core::source::{DiscoveryOptions, MigrationSet, discover};
use migrate_db_mssql::MssqlProvider;
use migrate_db_mssql::connstr::ConnectOptions;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ResultSet, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::EncryptionSetting;
use mssql_tds::datatypes::column_values::ColumnValues;

fn connection() -> String {
    std::env::var("MIGRATE_DB_TEST_MSSQL")
        .expect("set MIGRATE_DB_TEST_MSSQL to a disposable SQL Server connection string")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique schema per test; migrations create their objects in it.
fn unique_schema() -> String {
    format!(
        "it_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn ledger(schema: &str) -> toml::Table {
    toml::from_str(&format!("schema = \"{schema}\"")).unwrap()
}

async fn provider_named(schema: &str, app: &str) -> MssqlProvider {
    MssqlProvider::connect(&connection(), &ledger(schema), app, None)
        .await
        .unwrap()
}

async fn provider(schema: &str) -> MssqlProvider {
    provider_named(schema, "migrate-db/test").await
}

/// A plain driver connection for assertions (independent of the provider).
async fn raw() -> TdsClient {
    let o = ConnectOptions::parse(&connection(), None).unwrap();
    let mut ctx = ClientContext::default();
    ctx.database = o.database.clone();
    ctx.application_name = "migrate-db/assert".into();
    ctx.tds_authentication_method = TdsAuthenticationMethod::Password;
    ctx.user_name = o.user.clone().unwrap();
    ctx.password = o.password.clone().unwrap();
    ctx.encryption_options.mode = EncryptionSetting::On;
    ctx.encryption_options.trust_server_certificate = o.trust_server_certificate;
    TdsConnectionProvider {}
        .create_client(ctx, &o.data_source(), None)
        .await
        .unwrap()
}

/// First column of the first row, as an integer.
async fn scalar(sql: &str) -> i64 {
    let mut c = raw().await;
    c.execute(sql.into(), ()).await.unwrap();
    let row = c.next_row().await.unwrap().expect("a row");
    c.close_query().await.unwrap();
    match row.first() {
        Some(ColumnValues::Int(v)) => i64::from(*v),
        Some(ColumnValues::BigInt(v)) => *v,
        other => panic!("unexpected {other:?}"),
    }
}

async fn object_exists(name: &str) -> bool {
    scalar(&format!(
        "SELECT CASE WHEN OBJECT_ID(N'{name}') IS NULL THEN 0 ELSE 1 END"
    ))
    .await
        == 1
}

struct Migrations {
    _dir: tempfile::TempDir,
    set: MigrationSet,
}

/// `(version, up, down)`.
fn migrations(files: &[(&str, &str, Option<&str>)]) -> Migrations {
    let dir = tempfile::tempdir().unwrap();
    for (version, up, down) in files {
        fs::write(
            dir.path().join(format!("V{version}__m{version}.up.sql")),
            up,
        )
        .unwrap();
        if let Some(down) = down {
            fs::write(
                dir.path().join(format!("V{version}__m{version}.down.sql")),
                down,
            )
            .unwrap();
        }
    }
    let set = discover(
        dir.path(),
        &DiscoveryOptions {
            extensions: vec!["sql".into()],
            strict_filenames: false,
        },
    )
    .unwrap();
    Migrations { _dir: dir, set }
}

/// Migration 1 for most tests. The test schema already exists: it is the
/// ledger schema, which `ensure_ledger` creates before any step runs.
fn schema_migration(schema: &str) -> String {
    format!("CREATE TABLE [{schema}].a (id int);\nGO\n")
}

fn identity() -> RunIdentity {
    RunIdentity {
        applied_by: "it@host".into(),
        host: "host".into(),
        tool_version: "0.0.0-it".into(),
    }
}

fn lock(ms: u64) -> LockOptions {
    LockOptions {
        timeout: Duration::from_millis(ms),
        owner: "it".into(),
    }
}

const ALLOW_DOWN: Policy = Policy {
    allow_down: true,
    allow_out_of_order: false,
};

async fn run(
    p: &mut MssqlProvider,
    set: &MigrationSet,
    request: Request,
) -> Result<RunReport, Error> {
    Engine::new(p, set, ALLOW_DOWN, identity())
        .run(&request, &lock(5_000), None, &NoopObserver)
        .await
}

fn up() -> Request {
    Request::Up {
        to: None,
        steps: None,
    }
}

async fn history_count(schema: &str, filter: &str) -> i64 {
    scalar(&format!(
        "SELECT COUNT(*) FROM [{schema}].[__migrations_history] WHERE {filter}"
    ))
    .await
}

#[tokio::test]
async fn read_ledger_on_a_fresh_database_creates_nothing() {
    let schema = unique_schema();
    let mut p = provider(&schema).await;
    assert!(p.read_ledger().await.unwrap().is_empty());
    assert!(!object_exists(&format!("[{schema}].[__migrations]")).await);
}

#[tokio::test]
async fn up_runs_go_batches_atomically_then_down_reverts() {
    let schema = unique_schema();
    let m = migrations(&[
        (
            "1",
            &schema_migration(&schema),
            // The ledger tables live in this schema, so the down keeps it.
            Some(&format!("DROP TABLE [{schema}].a;\n")),
        ),
        (
            "2",
            // CREATE PROCEDURE must be first in its batch; GO 2 repeats a batch.
            &format!(
                "ALTER TABLE [{schema}].a ADD status nvarchar(20) NULL;\nGO\n\
                 CREATE PROCEDURE [{schema}].get_a AS SELECT id, status FROM [{schema}].a;\nGO\n\
                 INSERT [{schema}].a (id) VALUES (1);\nGO 2\n"
            ),
            Some(&format!(
                "DROP PROCEDURE [{schema}].get_a;\nGO\nDELETE [{schema}].a;\nALTER TABLE [{schema}].a DROP COLUMN status;\n"
            )),
        ),
    ]);
    let mut p = provider(&schema).await;

    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(object_exists(&format!("[{schema}].get_a")).await);
    assert_eq!(
        scalar(&format!("SELECT COUNT(*) FROM [{schema}].a")).await,
        2,
        "GO 2"
    );
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|r| r.status == Status::Applied));
    assert!(
        ledger[0].applied_by.starts_with("sql:sa / "),
        "{}",
        ledger[0].applied_by
    );

    let again = run(&mut p, &m.set, up()).await.unwrap();
    assert!(again.plan.is_empty());

    run(
        &mut p,
        &m.set,
        Request::Down {
            to: Some("0".parse().unwrap()),
            steps: None,
        },
    )
    .await
    .unwrap();
    assert!(!object_exists(&format!("[{schema}].a")).await);
    assert!(!object_exists(&format!("[{schema}].get_a")).await);
    assert!(p.read_ledger().await.unwrap().is_empty());
    assert_eq!(
        history_count(&schema, "status = 'succeeded'").await,
        4,
        "2 ups + 2 downs"
    );
}

#[tokio::test]
async fn failing_transactional_migration_rolls_back_completely() {
    let schema = unique_schema();
    let m = migrations(&[
        ("1", &schema_migration(&schema), None),
        (
            "2",
            &format!(
                "CREATE TABLE [{schema}].b (id int);\nGO\n-- line 3\nINSERT [{schema}].b VALUES ('not an int');\n"
            ),
            None,
        ),
    ]);
    let mut p = provider(&schema).await;

    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("step 2 must fail");
    assert_eq!(failure.version.as_str(), "2");
    assert!(failure.error.contains("Msg 245"), "{}", failure.error);
    assert!(
        failure.error.contains("batch 2 (starting at line 3)") && failure.error.contains("Line 4"),
        "file line numbers: {}",
        failure.error
    );
    assert!(
        object_exists(&format!("[{schema}].a")).await,
        "step 1 stays"
    );
    assert!(
        !object_exists(&format!("[{schema}].b")).await,
        "step 2 rolled back"
    );
    assert_eq!(p.read_ledger().await.unwrap().len(), 1);
    assert_eq!(
        history_count(&schema, "status = 'failed' AND version = '2'").await,
        1,
        "failure recorded outside the rolled-back transaction"
    );
}

#[tokio::test]
async fn parse_error_mid_transaction_rolls_back_despite_xact_abort() {
    // XACT_ABORT doesn't roll back on compile errors (ADR-0004): the provider must.
    let schema = unique_schema();
    let m = migrations(&[
        ("1", &schema_migration(&schema), None),
        (
            "2",
            &format!("CREATE TABLE [{schema}].b (id int);\nGO\nSELECT FROM WHERE;\n"),
            None,
        ),
        ("3", &format!("CREATE TABLE [{schema}].c (id int);\n"), None),
    ]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("step 2 must fail");
    // Msg 156 "Incorrect syntax near FROM": a compile error (level 15).
    assert!(failure.error.contains("Level 15"), "{}", failure.error);
    assert!(
        !object_exists(&format!("[{schema}].b")).await,
        "rolled back"
    );

    // The same session is clean: a fixed migration set runs and commits.
    let fixed = migrations(&[
        ("1", &schema_migration(&schema), None),
        ("2", &format!("CREATE TABLE [{schema}].b (id int);\n"), None),
    ]);
    let report = run(&mut p, &fixed.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(object_exists(&format!("[{schema}].b")).await);
}

#[tokio::test]
async fn error_in_a_later_statement_of_a_batch_is_not_lost() {
    let schema = unique_schema();
    let m = migrations(&[(
        "1",
        &format!("{}SELECT 1;\nSELECT 1/0;\n", schema_migration(&schema)),
        None,
    )]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("divide by zero must fail the step");
    assert!(failure.error.contains("Msg 8134"), "{}", failure.error);
    assert!(
        !object_exists(&format!("[{schema}].a")).await,
        "rolled back"
    );
}

#[tokio::test]
async fn scripts_that_break_the_transaction_fail_closed() {
    // (case, script after `CREATE TABLE a; GO`, expected text, dirty?)
    // Dirty: the script committed work itself, so the step is marked failed
    // and blocks later runs. Clean: everything was rolled back.
    for (name, body, why, dirty) in [
        ("commits", "COMMIT;", "committed or rolled back", true),
        (
            "rolls back, then keeps going",
            "ROLLBACK;\nCREATE TABLE b_outside (id int);",
            "committed or rolled back",
            true,
        ),
        (
            "commits, then fails in the same batch",
            "COMMIT;\nSELECT 1/0;",
            "Msg 8134",
            true,
        ),
        (
            "leaves one open",
            "BEGIN TRANSACTION;",
            "extra transaction",
            false,
        ),
        (
            "swallows a doomed error",
            "BEGIN TRY SELECT 1/0; END TRY BEGIN CATCH END CATCH;",
            // The server itself rolls a doomed transaction back at batch end.
            "Msg 3998",
            false,
        ),
    ] {
        let schema = unique_schema();
        let m = migrations(&[("1", &format!("{}{body}\n", schema_migration(&schema)), None)]);
        let mut p = provider(&schema).await;
        let report = run(&mut p, &m.set, up()).await.unwrap();
        let failure = report
            .failure
            .unwrap_or_else(|| panic!("{name}: must fail"));
        assert!(failure.error.contains(why), "{name}: {}", failure.error);
        let ledger = p.read_ledger().await.unwrap();
        if dirty {
            assert!(
                failure.error.contains("repair"),
                "{name}: {}",
                failure.error
            );
            assert_eq!(ledger.len(), 1, "{name}: a failed row blocks later runs");
            assert_eq!(ledger[0].status, Status::Failed, "{name}");
            let err = run(&mut p, &m.set, up()).await.unwrap_err();
            assert!(
                matches!(err, Error::Plan(PlanError::Unresolved { .. })),
                "{name}: {err}"
            );
        } else {
            assert!(ledger.is_empty(), "{name}: nothing recorded");
            assert!(
                !object_exists(&format!("[{schema}].a")).await,
                "{name}: rolled back"
            );
        }
        let _ =
            scalar("IF OBJECT_ID(N'dbo.b_outside') IS NOT NULL DROP TABLE dbo.b_outside; SELECT 0")
                .await;
    }
}

#[tokio::test]
async fn no_transaction_step_runs_and_failure_leaves_a_failed_row() {
    let schema = unique_schema();
    let ok = migrations(&[
        ("1", &schema_migration(&schema), None),
        (
            "2",
            &format!("-- migrate:no-transaction\nCREATE INDEX ix_a ON [{schema}].a (id);\n"),
            None,
        ),
    ]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &ok.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);

    let bad = migrations(&[
        ("1", &schema_migration(&schema), None),
        (
            "2",
            &format!("-- migrate:no-transaction\nCREATE INDEX ix_a ON [{schema}].a (id);\n"),
            None,
        ),
        (
            "3",
            "-- migrate:no-transaction\nSELECT * FROM missing_table;\n",
            None,
        ),
    ]);
    let report = run(&mut p, &bad.set, up()).await.unwrap();
    assert!(!report.succeeded());
    let ledger = p.read_ledger().await.unwrap();
    let row = ledger.iter().find(|r| r.version == "3").unwrap();
    assert_eq!(
        (row.status, row.direction),
        (Status::Failed, Some(Direction::Up))
    );

    let err = run(&mut p, &bad.set, up()).await.unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::Unresolved { .. })),
        "{err}"
    );
}

#[tokio::test]
async fn second_run_times_out_and_names_the_holder() {
    let schema = unique_schema();
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut holder = provider_named(&schema, "migrate-db/holder-run-42").await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();

    let mut waiter = provider(&schema).await;
    let err = Engine::new(&mut waiter, &m.set, ALLOW_DOWN, identity())
        .run(&up(), &lock(600), None, &NoopObserver)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), exit::LOCK_TIMEOUT);
    assert!(err.to_string().contains("holder-run-42"), "{err}");

    holder.release_lock().await.unwrap();
    assert!(run(&mut waiter, &m.set, up()).await.unwrap().succeeded());
}

#[tokio::test]
async fn lock_is_released_when_the_holder_disconnects() {
    let schema = unique_schema();
    let mut holder = provider(&schema).await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();
    drop(holder);
    let mut next = provider(&schema).await;
    next.acquire_lock(&lock(5_000)).await.unwrap();
    next.release_lock().await.unwrap();
}

#[tokio::test]
async fn timeout_directive_cancels_the_batch_and_the_session_stays_usable() {
    let schema = unique_schema();
    let m = migrations(&[
        ("1", &schema_migration(&schema), None),
        (
            "2",
            &format!(
                "-- migrate:timeout 1\nCREATE TABLE [{schema}].b (id int);\nWAITFOR DELAY '00:00:10';\n"
            ),
            None,
        ),
    ]);
    let mut p = provider(&schema).await;
    let started = std::time::Instant::now();
    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("must time out");
    assert!(failure.error.contains("timed out"), "{}", failure.error);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "cancelled, not waited out"
    );
    assert!(
        !object_exists(&format!("[{schema}].b")).await,
        "rolled back"
    );
    assert_eq!(
        p.read_ledger().await.unwrap().len(),
        1,
        "session still usable"
    );
}

#[tokio::test]
async fn non_transactional_step_killed_mid_script_leaves_an_in_progress_row() {
    let schema = unique_schema();
    let app = format!("migrate-db/kill-{schema}");
    let m = migrations(&[(
        "1",
        "-- migrate:no-transaction\nWAITFOR DELAY '00:00:30';\n",
        None,
    )]);
    let mut p = provider_named(&schema, &app).await;
    let killer = async {
        // Kill the script session (not the lock session) once it's waiting.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let n = scalar(&format!(
                "DECLARE @s int = (SELECT TOP (1) r.session_id FROM sys.dm_exec_requests r \
                 JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id \
                 WHERE s.program_name = N'{app}' AND r.wait_type = N'WAITFOR'); \
                 IF @s IS NOT NULL BEGIN DECLARE @k nvarchar(20) = N'KILL ' + CAST(@s AS nvarchar(10)); EXEC (@k); END; \
                 SELECT CASE WHEN @s IS NULL THEN 0 ELSE 1 END"
            ))
            .await;
            if n == 1 {
                return;
            }
        }
        panic!("script session never started waiting");
    };
    let (result, ()) = tokio::join!(run(&mut p, &m.set, up()), killer);
    assert!(
        result.is_err() || !result.as_ref().unwrap().succeeded(),
        "the run must not report success"
    );

    let mut fresh = provider(&schema).await;
    let ledger = fresh.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        (ledger[0].status, ledger[0].direction),
        (Status::InProgress, Some(Direction::Up))
    );
    let err = run(&mut fresh, &m.set, up()).await.unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::Unresolved { .. })),
        "{err}"
    );
}

#[tokio::test]
async fn holder_record_is_visible_while_held_and_stale_after_a_crash() {
    let schema = unique_schema();
    // A first run creates the ledger, so the record can be written at acquire.
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut p = provider(&schema).await;
    assert!(run(&mut p, &m.set, up()).await.unwrap().succeeded());

    let mut observer = provider(&schema).await;
    let free = observer.lock_status().await.unwrap();
    assert!(
        !free.held && free.holder.is_none() && !free.stale_info,
        "{free:?}"
    );

    let mut holder = provider_named(&schema, "migrate-db/holder-run-7").await;
    holder
        .acquire_lock(&LockOptions {
            timeout: Duration::from_secs(1),
            owner: "builder@agent-07".into(),
        })
        .await
        .unwrap();
    let held = observer.lock_status().await.unwrap();
    let record = held.holder.clone().expect("holder record");
    assert!(held.held && !held.stale_info);
    assert_eq!(record.owner.os_user, "builder@agent-07");
    assert_eq!(record.principal, "sa");
    assert_eq!(record.application, "migrate-db/holder-run-7");
    assert!(record.session_id.is_some());

    // The timeout message names the holder from the record.
    let err = provider(&schema)
        .await
        .acquire_lock(&lock(300))
        .await
        .unwrap_err();
    assert!(err.message.contains("builder@agent-07"), "{}", err.message);

    holder.release_lock().await.unwrap();
    let after = observer.lock_status().await.unwrap();
    assert!(
        !after.held && after.holder.is_none(),
        "record removed: {after:?}"
    );

    // A run that dies while holding the lock: the session ends, the record stays.
    let mut crashed = provider(&schema).await;
    crashed.acquire_lock(&lock(1_000)).await.unwrap();
    drop(crashed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stale = observer.lock_status().await.unwrap();
    assert!(
        !stale.held && stale.stale_info && stale.holder.is_some(),
        "{stale:?}"
    );

    // The next run takes over and replaces the stale record.
    assert!(run(&mut p, &m.set, up()).await.unwrap().succeeded());
    assert!(!observer.lock_status().await.unwrap().stale_info);
}

#[tokio::test]
async fn lock_status_on_a_fresh_database_creates_nothing() {
    let schema = unique_schema();
    let mut p = provider(&schema).await;
    let s = p.lock_status().await.unwrap();
    assert!(!s.held && s.holder.is_none());
    assert!(!object_exists(&format!("[{schema}].[__migrations_meta]")).await);
}

#[tokio::test]
async fn invalid_go_line_fails_the_run_before_any_step() {
    let schema = unique_schema();
    let m = migrations(&[
        ("1", &schema_migration(&schema), None),
        ("2", "SELECT 2;\nGO 0\n", None),
    ]);
    let mut p = provider(&schema).await;
    let err = run(&mut p, &m.set, up()).await.unwrap_err();
    assert_eq!(err.exit_code(), exit::VALIDATION, "{err}");
    assert!(err.to_string().contains("V2__m2.up.sql: line 2"), "{err}");
    assert!(
        !object_exists(&format!("[{schema}].a")).await,
        "step 1 didn't run either"
    );
}

/// Opt-in cloud test (AGENTS.md § Safety): set MIGRATE_DB_TEST_AZURE_SQL to an
/// Entra connection string for a database you may read, and sign in with
/// `az login`. Read-only: no ledger, no lock, nothing is created.
#[tokio::test]
async fn entra_sign_in_against_azure_sql_is_read_only() {
    let Ok(conn) = std::env::var("MIGRATE_DB_TEST_AZURE_SQL") else {
        eprintln!("skipped: MIGRATE_DB_TEST_AZURE_SQL not set");
        return;
    };
    let schema = unique_schema();
    let mut p = MssqlProvider::connect(
        &conn,
        &ledger(&schema),
        "migrate-db/entra-test",
        Some("entra"),
    )
    .await
    .unwrap();
    assert!(
        p.read_ledger().await.unwrap().is_empty(),
        "fresh schema: no ledger"
    );
    let lock = p.lock_status().await.unwrap();
    assert!(!lock.held);
}

/// Versioned files plus `R__<name>.sql` repeatables in one folder.
fn migrations_with_repeatables(
    files: &[(&str, &str, Option<&str>)],
    repeatables: &[(&str, &str)],
) -> Migrations {
    let m = migrations(files);
    for (name, body) in repeatables {
        fs::write(m._dir.path().join(format!("R__{name}.sql")), body).unwrap();
    }
    rediscover(m)
}

fn rediscover(m: Migrations) -> Migrations {
    let set = discover(
        m._dir.path(),
        &DiscoveryOptions {
            extensions: vec!["sql".into()],
            strict_filenames: false,
        },
    )
    .unwrap();
    Migrations { _dir: m._dir, set }
}

async fn proc_result(schema: &str) -> i64 {
    let mut c = raw().await;
    c.execute(format!("EXEC [{schema}].answer;"), ())
        .await
        .unwrap();
    let row = c.next_row().await.unwrap().expect("a row");
    c.close_query().await.unwrap();
    match row.first() {
        Some(ColumnValues::Int(v)) => i64::from(*v),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn repeatables_apply_after_versioned_and_reapply_only_when_changed() {
    let schema = unique_schema();
    let proc = |n: i32| {
        format!("CREATE OR ALTER PROCEDURE [{schema}].answer AS SELECT {n} AS answer;\nGO\n")
    };
    let m = migrations_with_repeatables(
        &[(
            "1",
            &schema_migration(&schema),
            Some(&format!("DROP TABLE [{schema}].a;")),
        )],
        &[("answer", &proc(41))],
    );
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    let ids: Vec<String> = report
        .completed
        .iter()
        .map(|s| s.version.to_string())
        .collect();
    assert_eq!(ids, ["1", "R__answer"]);
    assert_eq!(proc_result(&schema).await, 41);
    let ledger = p.read_ledger().await.unwrap();
    let rep = ledger.iter().find(|r| r.version == "R__answer").unwrap();
    assert_eq!(rep.kind, migrate_db_core::model::Kind::Repeatable);

    // Unchanged: nothing to do.
    assert!(run(&mut p, &m.set, up()).await.unwrap().plan.is_empty());

    // Changed: re-applied, same ledger row updated.
    fs::write(m._dir.path().join("R__answer.sql"), proc(42)).unwrap();
    let m = rediscover(m);
    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert_eq!(report.completed.len(), 1);
    assert_eq!(proc_result(&schema).await, 42);
    let rows = p.read_ledger().await.unwrap();
    assert_eq!(rows.iter().filter(|r| r.version == "R__answer").count(), 1);

    // `down` never touches repeatables.
    let report = run(
        &mut p,
        &m.set,
        Request::Down {
            to: Some("0".parse().unwrap()),
            steps: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.completed.len(), 1, "only the versioned migration");
    assert!(object_exists(&format!("[{schema}].answer")).await);
}

#[tokio::test]
async fn failing_repeatable_rolls_back_and_is_retried() {
    let schema = unique_schema();
    let m = migrations_with_repeatables(
        &[("1", &schema_migration(&schema), None)],
        &[(
            "bad",
            "CREATE OR ALTER VIEW dbo.v_bad AS SELECT nope FROM nowhere;\n",
        )],
    );
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("the repeatable fails");
    assert_eq!(failure.version.as_str(), "R__bad");
    assert_eq!(
        report.completed.len(),
        1,
        "the versioned migration stays applied"
    );
    let ledger = p.read_ledger().await.unwrap();
    assert!(
        ledger.iter().all(|r| r.version != "R__bad"),
        "transactional: no row"
    );

    // Fixed file: applied on the next run.
    fs::write(
        m._dir.path().join("R__bad.sql"),
        format!("CREATE OR ALTER VIEW [{schema}].v_ok AS SELECT id FROM [{schema}].a;\n"),
    )
    .unwrap();
    let m = rediscover(m);
    assert!(run(&mut p, &m.set, up()).await.unwrap().succeeded());
    assert!(object_exists(&format!("[{schema}].v_ok")).await);
}

#[tokio::test]
async fn cancelling_interrupts_the_running_statement_and_rolls_back() {
    use migrate_db_core::cancel::CancelToken;
    use migrate_db_core::engine::FailureKind;
    let schema = unique_schema();
    let m = migrations(&[(
        "1",
        &format!("CREATE TABLE [{schema}].c (id int);\nWAITFOR DELAY '00:00:30';\n"),
        None,
    )]);
    let mut p = provider(&schema).await;
    let token = CancelToken::new();
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        canceller.cancel();
    });
    let started = std::time::Instant::now();
    let report = Engine::new(&mut p, &m.set, ALLOW_DOWN, identity())
        .with_cancel(token)
        .run(&up(), &lock(5_000), None, &NoopObserver)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "interrupted, not waited out"
    );
    let failure = report.failure.as_ref().expect("cancelled step");
    assert_eq!(failure.kind, FailureKind::Cancelled, "{}", failure.error);
    assert_eq!(report.exit_code(), exit::MIGRATION_FAILED);
    assert!(
        !object_exists(&format!("[{schema}].c")).await,
        "rolled back"
    );
    assert!(
        p.read_ledger().await.unwrap().is_empty(),
        "nothing recorded"
    );
    let mut next = provider(&schema).await;
    next.acquire_lock(&lock(2_000)).await.unwrap();
    next.release_lock().await.unwrap();
}

#[tokio::test]
async fn force_unlock_clears_stale_records_and_kills_the_verified_holder() {
    use migrate_db_core::provider::UnlockOutcome;
    let schema = unique_schema();
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut admin = provider(&schema).await;
    assert!(run(&mut admin, &m.set, up()).await.unwrap().succeeded());

    // Free, no record: nothing to do.
    assert_eq!(
        unlock(&mut admin, false).await.unwrap(),
        UnlockOutcome::NothingToDo
    );

    // A crashed run's stale record is removed, and audited.
    let mut crashed = provider(&schema).await;
    crashed.acquire_lock(&lock(1_000)).await.unwrap();
    drop(crashed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(admin.lock_status().await.unwrap().stale_info);
    assert!(matches!(
        unlock(&mut admin, false).await.unwrap(),
        UnlockOutcome::ClearedStale(Some(_))
    ));
    let after = admin.lock_status().await.unwrap();
    assert!(!after.held && after.holder.is_none(), "{after:?}");
    assert_eq!(history_count(&schema, "event = N'force-unlock'").await, 1);

    // Held: refused without --kill-holder; the holder keeps the lock.
    let mut holder = provider_named(&schema, "migrate-db/holder-to-kill").await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();
    let err = unlock(&mut admin, false).await.unwrap_err();
    assert_eq!(Error::from(err).exit_code(), exit::LOCK_TIMEOUT);
    assert!(admin.lock_status().await.unwrap().held);

    // --kill-holder (sa may see the DMV and KILL): the session ends and the
    // next run gets the lock at once.
    let killed = unlock(&mut admin, true).await.unwrap();
    assert!(
        matches!(&killed, UnlockOutcome::KilledHolder(h) if h.application == "migrate-db/holder-to-kill"),
        "{killed:?}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = admin.lock_status().await.unwrap();
    assert!(!after.held && after.holder.is_none(), "{after:?}");
    assert_eq!(history_count(&schema, "event = N'force-unlock'").await, 2);
    let mut next = provider(&schema).await;
    next.acquire_lock(&lock(100)).await.unwrap();
    next.release_lock().await.unwrap();
    // The killed run's own release fails; it never removes someone else's record.
    assert!(holder.release_lock().await.is_err());
}

async fn unlock(
    p: &mut MssqlProvider,
    kill_holder: bool,
) -> Result<migrate_db_core::provider::UnlockOutcome, migrate_db_core::provider::ProviderError> {
    p.force_unlock(kill_holder, &identity()).await
}

#[tokio::test]
async fn kill_holder_refuses_a_session_it_cannot_confirm() {
    let schema = unique_schema();
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut admin = provider(&schema).await;
    assert!(run(&mut admin, &m.set, up()).await.unwrap().succeeded());
    let mut holder = provider(&schema).await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();
    // The record names a session that doesn't hold the lock (here: the
    // assertion connection itself, which is alive and must not be killed).
    scalar(&format!(
        "UPDATE [{schema}].[__migrations_meta] SET value = JSON_MODIFY(value, '$.session_id', @@SPID) \
         OUTPUT 1 WHERE [key] = N'lock_holder'"
    ))
    .await;
    let err = unlock(&mut admin, true).await.unwrap_err();
    assert!(
        err.message.contains("can't be confirmed"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("nothing was killed"),
        "{}",
        err.message
    );
    assert!(
        admin.lock_status().await.unwrap().held,
        "the holder is untouched"
    );
    holder.release_lock().await.unwrap();
}
