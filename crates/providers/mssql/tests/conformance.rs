//! The shared provider contract (`migrate-db-conformance`) against a real
//! SQL Server. Container setup: see `integration.rs`.
#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code

use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use migrate_db_conformance::{Harness, HistoryRow, Target, conformance_tests};
use migrate_db_core::provider::Provider;
use migrate_db_mssql::MssqlProvider;
use migrate_db_mssql::connstr::ConnectOptions;

use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{ResultSet as _, TdsClient};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::EncryptionSetting;
use mssql_tds::datatypes::column_values::ColumnValues;

struct SqlServer {
    connection: String,
}

impl SqlServer {
    fn from_env() -> Self {
        Self {
            connection: std::env::var("MIGRATE_DB_TEST_MSSQL")
                .expect("set MIGRATE_DB_TEST_MSSQL to a disposable SQL Server connection string"),
        }
    }

    /// A plain driver connection for assertions.
    async fn raw(&self) -> TdsClient {
        let o = ConnectOptions::parse(&self.connection, None).unwrap();
        let mut ctx = ClientContext::default();
        ctx.database = o.database.clone();
        ctx.application_name = "migrate-db/conformance-assert".into();
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

    /// Every row of the first result set, as text.
    async fn rows(&self, sql: &str) -> Vec<Vec<String>> {
        let mut c = self.raw().await;
        c.execute(sql.into(), ()).await.unwrap();
        let mut rows = Vec::new();
        while let Some(row) = c.next_row().await.unwrap() {
            rows.push(row.iter().map(text).collect());
        }
        c.close_query().await.unwrap();
        rows
    }

    async fn scalar(&self, sql: &str) -> String {
        self.rows(sql).await.swap_remove(0).swap_remove(0)
    }
}

fn text(v: &ColumnValues) -> String {
    match v {
        ColumnValues::Null => String::new(),
        ColumnValues::Int(i) => i.to_string(),
        ColumnValues::BigInt(i) => i.to_string(),
        ColumnValues::String(s) => s.to_utf8_string(),
        other => format!("{other:?}"),
    }
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

#[async_trait]
impl Harness for SqlServer {
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
            MssqlProvider::connect(&self.connection, &ledger, application, None)
                .await
                .unwrap(),
        )
    }

    async fn table_exists(&self, t: &Target, name: &str) -> bool {
        self.scalar(&format!(
            "SELECT CASE WHEN OBJECT_ID(N'[{}].[{name}]', N'U') IS NULL THEN 0 ELSE 1 END",
            t.namespace
        ))
        .await
            == "1"
    }

    async fn ledger_storage_exists(&self, t: &Target) -> bool {
        self.scalar(&format!(
            "SELECT COUNT(*) FROM sys.tables WHERE SCHEMA_NAME(schema_id) = N'{}' \
             AND name LIKE N'[_][_]migrations%'",
            t.namespace
        ))
        .await
            != "0"
    }

    async fn history(&self, t: &Target) -> Vec<HistoryRow> {
        self.rows(&format!(
            "IF OBJECT_ID(N'[{0}].[__migrations_history]', N'U') IS NULL SELECT TOP (0) N'' a, N'' b, N'' c; \
             ELSE SELECT version, event, status FROM [{0}].[__migrations_history] ORDER BY id",
            t.namespace
        ))
        .await
        .into_iter()
        .map(|r| HistoryRow {
            version: r[0].clone(),
            event: r[1].clone(),
            status: r[2].clone(),
        })
        .collect()
    }

    async fn point_holder_at_another_session(&self, t: &Target) {
        let changed = self
            .rows(&format!(
                "UPDATE [{}].[__migrations_meta] SET value = JSON_MODIFY(value, '$.session_id', @@SPID) \
                 OUTPUT 1 WHERE [key] = N'lock_holder'",
                t.namespace
            ))
            .await;
        assert_eq!(changed.len(), 1, "a holder record to rewrite");
    }

    fn create_table(&self, t: &Target, name: &str) -> String {
        format!("CREATE TABLE [{}].[{name}] (id int);\n", t.namespace)
    }

    fn drop_table(&self, t: &Target, name: &str) -> String {
        format!("DROP TABLE [{}].[{name}];\n", t.namespace)
    }

    fn create_then_fail(&self, t: &Target, name: &str) -> String {
        format!(
            "CREATE TABLE [{0}].[{name}] (id int);\nGO\nINSERT [{0}].[{name}] VALUES ('not an int');\n",
            t.namespace
        )
    }

    fn create_then_sleep(&self, t: &Target, name: &str, secs: u32) -> String {
        format!(
            "CREATE TABLE [{}].[{name}] (id int);\nWAITFOR DELAY '00:00:{secs:02}';\n",
            t.namespace
        )
    }

    fn define_view(&self, t: &Target, name: &str, marker: i32) -> String {
        format!(
            "CREATE OR ALTER VIEW [{}].[{name}] AS SELECT {marker} AS marker;\n",
            t.namespace
        )
    }

    async fn view_returns(&self, t: &Target, name: &str, marker: i32) -> bool {
        let rows = self
            .rows(&format!(
                "IF OBJECT_ID(N'[{0}].[{name}]', N'V') IS NULL SELECT -1; \
                 ELSE SELECT marker FROM [{0}].[{name}]",
                t.namespace
            ))
            .await;
        rows.first().and_then(|r| r.first()) == Some(&marker.to_string())
    }
}

conformance_tests!(SqlServer::from_env());
