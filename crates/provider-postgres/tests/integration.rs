//! Integration tests against a real PostgreSQL.
//!
//! Run with a disposable server, e.g.:
//! ```text
//! podman run -d --name migrate-db-pg -e POSTGRES_PASSWORD=migrate-test \
//!   -e POSTGRES_DB=migrate_test -p 55432:5432 docker.io/library/postgres:17
//! MIGRATE_DB_TEST_POSTGRES="host=localhost port=55432 user=postgres password=migrate-test dbname=migrate_test" \
//!   cargo test -p migrate-db-postgres --features integration
//! ```
//! Every test uses its own schema, so tests can run in parallel.
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
use migrate_db_postgres::PostgresProvider;

fn connection() -> String {
    std::env::var("MIGRATE_DB_TEST_POSTGRES")
        .expect("set MIGRATE_DB_TEST_POSTGRES to a disposable PostgreSQL connection string")
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

async fn provider(schema: &str) -> PostgresProvider {
    PostgresProvider::connect(&connection(), &ledger(schema), "migrate-db/test")
        .await
        .unwrap()
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
    p: &mut PostgresProvider,
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

async fn table_exists(p: &PostgresProvider, schema: &str, table: &str) -> bool {
    let client = tokio_postgres_client(p).await;
    client
        .query_one(
            "SELECT to_regclass($1) IS NOT NULL",
            &[&format!("\"{schema}\".\"{table}\"")],
        )
        .await
        .unwrap()
        .get(0)
}

/// A separate plain client for assertions.
async fn tokio_postgres_client(_: &PostgresProvider) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&connection(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    client
}

#[tokio::test]
async fn read_ledger_on_a_fresh_database_creates_nothing() {
    let schema = unique_schema();
    let mut p = provider(&schema).await;
    assert!(p.read_ledger().await.unwrap().is_empty());
    assert!(!table_exists(&p, &schema, "__migrations").await);
}

#[tokio::test]
async fn up_applies_scripts_and_ledger_atomically_then_down_reverts() {
    let schema = unique_schema();
    let m = migrations(&[
        (
            "1",
            &format!(
                "CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.orders (id int primary key);"
            ),
            Some(&format!("DROP TABLE {schema}.orders;")),
        ),
        (
            "2",
            &format!("ALTER TABLE {schema}.orders ADD COLUMN status text;"),
            Some(&format!("ALTER TABLE {schema}.orders DROP COLUMN status;")),
        ),
    ]);
    let mut p = provider(&schema).await;

    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(table_exists(&p, &schema, "orders").await);
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|r| r.status == Status::Applied));
    assert!(
        ledger[0].applied_by.starts_with("pg:"),
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
    assert!(!table_exists(&p, &schema, "orders").await);
    assert!(p.read_ledger().await.unwrap().is_empty());

    let client = tokio_postgres_client(&p).await;
    let history: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM \"{schema}\".\"__migrations_history\""),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(history, 4, "2 ups + 2 downs");
}

#[tokio::test]
async fn failing_transactional_migration_rolls_back_completely() {
    let schema = unique_schema();
    let m = migrations(&[
        (
            "1",
            &format!("CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.a (id int);"),
            None,
        ),
        (
            "2",
            &format!("CREATE TABLE {schema}.b (id int); SELECT * FROM {schema}.does_not_exist;"),
            None,
        ),
    ]);
    let mut p = provider(&schema).await;

    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("step 2 must fail");
    assert_eq!(failure.version.as_str(), "2");
    assert!(
        failure.error.contains("42P01"),
        "SQLSTATE kept: {}",
        failure.error
    );
    assert!(table_exists(&p, &schema, "a").await, "step 1 stays");
    assert!(!table_exists(&p, &schema, "b").await, "step 2 rolled back");
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 1);

    let client = tokio_postgres_client(&p).await;
    let failed: i64 = client
        .query_one(
            &format!("SELECT count(*) FROM \"{schema}\".\"__migrations_history\" WHERE status = 'failed' AND version = '2'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        failed, 1,
        "failure recorded outside the rolled-back transaction"
    );
}

#[tokio::test]
async fn no_transaction_directive_allows_create_index_concurrently() {
    let schema = unique_schema();
    let m = migrations(&[
        (
            "1",
            &format!("CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.t (c int);"),
            None,
        ),
        (
            "2",
            &format!("-- migrate:no-transaction\nCREATE INDEX CONCURRENTLY t_c ON {schema}.t (c);"),
            None,
        ),
    ]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(table_exists(&p, &schema, "t_c").await);
}

#[tokio::test]
async fn failing_non_transactional_migration_leaves_a_failed_row() {
    let schema = unique_schema();
    let m = migrations(&[(
        "1",
        "-- migrate:no-transaction\nCREATE INDEX CONCURRENTLY nope ON missing_table (c);",
        None,
    )]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    assert!(!report.succeeded());
    let ledger = p.read_ledger().await.unwrap();
    assert_eq!(ledger[0].status, Status::Failed);
    assert_eq!(ledger[0].direction, Some(Direction::Up));

    let err = run(&mut p, &m.set, up()).await.unwrap_err();
    assert!(matches!(err, Error::Plan(PlanError::Unresolved { .. })));
}

#[tokio::test]
async fn second_run_times_out_and_names_the_holder() {
    let schema = unique_schema();
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut holder =
        PostgresProvider::connect(&connection(), &ledger(&schema), "migrate-db/holder-run-42")
            .await
            .unwrap();
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
async fn statement_timeout_directive_is_enforced() {
    let schema = unique_schema();
    let m = migrations(&[("1", "-- migrate:timeout 1\nSELECT pg_sleep(3);", None)]);
    let mut p = provider(&schema).await;
    let report = run(&mut p, &m.set, up()).await.unwrap();
    let failure = report.failure.expect("must time out");
    assert!(
        failure.error.contains("57014"),
        "query_canceled: {}",
        failure.error
    );
}

#[tokio::test]
async fn non_transactional_step_killed_mid_script_leaves_an_in_progress_row() {
    let schema = unique_schema();
    // The script kills its own backend: the run dies mid-script, like a lost connection.
    let m = migrations(&[(
        "1",
        "-- migrate:no-transaction\nSELECT pg_terminate_backend(pg_backend_pid());",
        None,
    )]);
    let mut p = provider(&schema).await;
    let result = run(&mut p, &m.set, up()).await;
    assert!(
        result.is_err() || !result.as_ref().unwrap().succeeded(),
        "the run must not report success"
    );

    let mut fresh = provider(&schema).await;
    let ledger = fresh.read_ledger().await.unwrap();
    assert_eq!(ledger.len(), 1);
    assert_eq!(ledger[0].status, Status::InProgress);
    assert_eq!(ledger[0].direction, Some(Direction::Up));

    let err = run(&mut fresh, &m.set, up()).await.unwrap_err();
    assert!(
        matches!(err, Error::Plan(PlanError::Unresolved { .. })),
        "{err}"
    );
}

#[tokio::test]
async fn cancelling_interrupts_the_running_statement_and_rolls_back() {
    use migrate_db_core::cancel::CancelToken;
    use migrate_db_core::engine::FailureKind;
    let schema = unique_schema();
    let m = migrations(&[(
        "1",
        &format!(
            "CREATE SCHEMA IF NOT EXISTS {schema}; CREATE TABLE {schema}.c (id int); SELECT pg_sleep(30);"
        ),
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
    let failure = report.failure.expect("cancelled step");
    assert_eq!(failure.kind, FailureKind::Cancelled, "{}", failure.error);
    assert!(!table_exists(&p, &schema, "c").await, "rolled back");
    assert!(
        p.read_ledger().await.unwrap().is_empty(),
        "nothing recorded"
    );
}

async fn unlock(
    p: &mut PostgresProvider,
    kill_holder: bool,
) -> Result<migrate_db_core::provider::UnlockOutcome, migrate_db_core::provider::ProviderError> {
    p.force_unlock(kill_holder, &identity()).await
}

#[tokio::test]
async fn force_unlock_clears_stale_records_and_kills_the_verified_holder() {
    use migrate_db_core::provider::UnlockOutcome;
    let schema = unique_schema();
    let m = migrations(&[("1", "SELECT 1;", None)]);
    let mut admin = provider(&schema).await;
    assert!(run(&mut admin, &m.set, up()).await.unwrap().succeeded());
    let history = |filter: &str| {
        format!("SELECT count(*) FROM \"{schema}\".\"__migrations_history\" WHERE {filter}")
    };
    let check = tokio_postgres_client(&admin).await;

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
    assert!(admin.lock_status().await.unwrap().holder.is_none());
    let n: i64 = check
        .query_one(&history("event = 'force-unlock'"), &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);

    // Held: refused without --kill-holder.
    let mut holder = provider(&schema).await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();
    let err = unlock(&mut admin, false).await.unwrap_err();
    assert_eq!(Error::from(err).exit_code(), exit::LOCK_TIMEOUT);

    // A record naming another backend is never acted on.
    check
        .execute(
            &format!(
                "UPDATE \"{schema}\".\"__migrations_meta\" SET value = \
                 jsonb_set(value::jsonb, '{{session_id}}', to_jsonb(pg_backend_pid()))::text \
                 WHERE key = 'lock_holder'"
            ),
            &[],
        )
        .await
        .unwrap();
    let err = unlock(&mut admin, true).await.unwrap_err();
    assert!(
        err.message.contains("nothing was killed"),
        "{}",
        err.message
    );
    assert!(!check.is_closed(), "the named backend was not terminated");
    holder.release_lock().await.unwrap();

    // --kill-holder on the real holder (postgres is superuser).
    let mut holder = provider(&schema).await;
    holder.acquire_lock(&lock(1_000)).await.unwrap();
    assert!(matches!(
        unlock(&mut admin, true).await.unwrap(),
        UnlockOutcome::KilledHolder(_)
    ));
    let after = admin.lock_status().await.unwrap();
    assert!(!after.held && after.holder.is_none(), "{after:?}");
    let mut next = provider(&schema).await;
    next.acquire_lock(&lock(100)).await.unwrap();
    next.release_lock().await.unwrap();
}

/// `sslmode=require` must fail rather than fall back to plain text. The
/// disposable test server runs without TLS, so the connection is refused.
#[tokio::test]
async fn require_refuses_a_server_without_tls() {
    let schema = unique_schema();
    let conn = format!("{} sslmode=require", connection());
    let err = PostgresProvider::connect(&conn, &ledger(&schema), "migrate-db/test")
        .await
        .expect_err("a server without TLS must be refused under sslmode=require");
    assert!(
        err.to_string().contains("cannot connect"),
        "unexpected error: {err}"
    );
}

/// Opt-in cloud test (AGENTS.md § Safety): set MIGRATE_DB_TEST_AZURE_POSTGRES
/// to a connection string for an Azure Database for PostgreSQL Flexible
/// Server you may read, with `sslmode=require` (the server refuses plain
/// text). With Entra sign-in the password is an access token, for example
/// from `az account get-access-token --resource-type oss-rdbms`. Read-only:
/// no ledger, no lock, nothing is created.
#[tokio::test]
async fn tls_against_azure_flexible_server_is_read_only() {
    let Ok(conn) = std::env::var("MIGRATE_DB_TEST_AZURE_POSTGRES") else {
        eprintln!("skipped: MIGRATE_DB_TEST_AZURE_POSTGRES not set");
        return;
    };
    let schema = unique_schema();
    let mut p = PostgresProvider::connect(&conn, &ledger(&schema), "migrate-db/azure-test")
        .await
        .unwrap();
    assert!(
        p.read_ledger().await.unwrap().is_empty(),
        "fresh schema: no ledger"
    );
    let lock = p.lock_status().await.unwrap();
    assert!(!lock.held);
}
