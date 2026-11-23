//! The shared provider contract (`migrate-db-conformance`) against a real
//! PostgreSQL. Container setup: see `integration.rs`.
#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use migrate_db_conformance::{Harness, HistoryRow, Target, conformance_tests};
use migrate_db_core::provider::Provider;
use migrate_db_postgres::PostgresProvider;

struct Postgres {
    connection: String,
}

impl Postgres {
    fn from_env() -> Self {
        Self {
            connection: std::env::var("MIGRATE_DB_TEST_POSTGRES").expect(
                "set MIGRATE_DB_TEST_POSTGRES to a disposable PostgreSQL connection string",
            ),
        }
    }

    /// A plain client for assertions.
    async fn raw(&self) -> tokio_postgres::Client {
        let (client, conn) = tokio_postgres::connect(&self.connection, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
    }

    async fn exists(&self, sql: &str, args: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> bool {
        self.raw().await.query_one(sql, args).await.unwrap().get(0)
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

#[async_trait]
impl Harness for Postgres {
    fn new_target(&self) -> Target {
        Target {
            namespace: format!(
                "cf_{}_{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    async fn connect(&self, target: &Target, application: &str) -> Box<dyn Provider> {
        let ledger: toml::Table =
            toml::from_str(&format!("schema = \"{}\"", target.namespace)).unwrap();
        Box::new(
            PostgresProvider::connect(&self.connection, &ledger, application)
                .await
                .unwrap(),
        )
    }

    async fn table_exists(&self, t: &Target, name: &str) -> bool {
        self.exists(
            "SELECT to_regclass($1) IS NOT NULL",
            &[&format!("\"{}\".\"{name}\"", t.namespace)],
        )
        .await
    }

    async fn ledger_storage_exists(&self, t: &Target) -> bool {
        self.exists(
            "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = $1 \
             AND tablename LIKE '\\_\\_migrations%')",
            &[&t.namespace],
        )
        .await
    }

    async fn history(&self, t: &Target) -> Vec<HistoryRow> {
        let table = format!("\"{}\".\"__migrations_history\"", t.namespace);
        let client = self.raw().await;
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
            .await
            .unwrap()
            .get(0);
        if !exists {
            return Vec::new();
        }
        client
            .query(
                &format!("SELECT version, event, status FROM {table} ORDER BY id"),
                &[],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| HistoryRow {
                version: r.get(0),
                event: r.get(1),
                status: r.get(2),
            })
            .collect()
    }

    async fn point_holder_at_another_session(&self, t: &Target) {
        // The assertion backend's own pid: alive, and not the holder.
        let n = self
            .raw()
            .await
            .execute(
                &format!(
                    "UPDATE \"{}\".\"__migrations_meta\" SET value = \
                     jsonb_set(value::jsonb, '{{session_id}}', to_jsonb(pg_backend_pid()))::text \
                     WHERE key = 'lock_holder'",
                    t.namespace
                ),
                &[],
            )
            .await
            .unwrap();
        assert_eq!(n, 1, "a holder record to rewrite");
    }

    fn create_table(&self, t: &Target, name: &str) -> String {
        format!("CREATE TABLE \"{}\".\"{name}\" (id int);\n", t.namespace)
    }

    fn drop_table(&self, t: &Target, name: &str) -> String {
        format!("DROP TABLE \"{}\".\"{name}\";\n", t.namespace)
    }

    fn create_then_fail(&self, t: &Target, name: &str) -> String {
        format!(
            "CREATE TABLE \"{0}\".\"{name}\" (id int);\nINSERT INTO \"{0}\".\"{name}\" VALUES ('not an int');\n",
            t.namespace
        )
    }

    fn create_then_sleep(&self, t: &Target, name: &str, secs: u32) -> String {
        format!(
            "CREATE TABLE \"{}\".\"{name}\" (id int);\nSELECT pg_sleep({secs});\n",
            t.namespace
        )
    }

    fn define_view(&self, t: &Target, name: &str, marker: i32) -> String {
        format!(
            "CREATE OR REPLACE VIEW \"{}\".\"{name}\" AS SELECT {marker} AS marker;\n",
            t.namespace
        )
    }

    async fn view_returns(&self, t: &Target, name: &str, marker: i32) -> bool {
        let view = format!("\"{}\".\"{name}\"", t.namespace);
        let client = self.raw().await;
        let exists: bool = client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&view])
            .await
            .unwrap()
            .get(0);
        exists
            && client
                .query_one(&format!("SELECT marker FROM {view}"), &[])
                .await
                .unwrap()
                .get::<_, i32>(0)
                == marker
    }
}

conformance_tests!(Postgres::from_env());
