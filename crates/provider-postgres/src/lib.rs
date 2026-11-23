//! PostgreSQL provider for migrate-db (PROVIDERS.md § PostgreSQL).
//!
//! * One connection runs scripts and ledger writes; a second, dedicated
//!   connection holds the session-level advisory lock for the whole run.
//! * Transactional steps run the script **and** the ledger write in one
//!   transaction. `-- migrate:no-transaction` steps run outside one.
//! * The ledger schema is the full SPEC §6.1 shape from day one
//!   (`ledger_schema_version = 1`).
//! * TLS uses rustls and always verifies the server certificate and host
//!   name, against the Mozilla roots plus the operating system's store.
//!   `sslmode` keeps its driver meaning: `prefer` (the default) falls back
//!   to plain text only when the server has no TLS, `require` never does,
//!   and `disable` never tries TLS.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use sha2::{Digest, Sha256};
use tokio_postgres::{Client, Config, Row};
use tokio_postgres_rustls::MakeRustlsConnect;

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::model::{AppliedMigration, Direction, Kind, Status, truncate_error};
use migrate_db_core::planner::PlannedStep;
use migrate_db_core::provider::{
    Capabilities, ExecuteRequest, LockHolder, LockOptions, LockStatus, Provider, ProviderError,
    ProviderErrorKind, RunIdentity, StepOutcome, UnlockOutcome,
};

/// Ledger schema version this provider writes.
pub const LEDGER_SCHEMA_VERSION: i64 = 1;

/// Warnings for a repeatable (`R__`) script (SPEC §3.6): it re-runs
/// whenever it changes, so `CREATE` must be `CREATE OR REPLACE`.
pub fn lint_repeatable(script: &migrate_db_core::source::ScriptFile) -> Vec<String> {
    let code: String = script
        .content
        .lines()
        .map(|l| l.split("--").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    let words: Vec<&str> = code.split_whitespace().collect();
    let plain_create = words.windows(2).any(|w| {
        w[0] == "create"
            && matches!(
                w[1],
                "function" | "view" | "procedure" | "trigger" | "materialized"
            )
    });
    if plain_create {
        vec![format!(
            "{}: use CREATE OR REPLACE in repeatables; a plain CREATE fails when the file re-applies",
            script.relative_path
        )]
    } else {
        Vec::new()
    }
}

/// Ledger location (`ledger = { schema = …, table = … }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerLocation {
    /// Schema (default `public`).
    pub schema: String,
    /// Applied-set table (default `__migrations`); history and meta tables
    /// are `<table>_history` and `<table>_meta`.
    pub table: String,
}

impl Default for LedgerLocation {
    fn default() -> Self {
        Self {
            schema: "public".into(),
            table: "__migrations".into(),
        }
    }
}

impl LedgerLocation {
    /// Read from the target's `ledger` table, validating identifiers.
    pub fn from_config(ledger: &toml::Table) -> Result<Self, ProviderError> {
        let mut loc = Self::default();
        for (key, value) in ledger {
            let text = value.as_str().ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::Other,
                    format!("ledger.{key} must be a string"),
                )
            })?;
            match key.as_str() {
                "schema" => loc.schema = text.to_owned(),
                "table" => loc.table = text.to_owned(),
                other => {
                    return Err(ProviderError::new(
                        ProviderErrorKind::Other,
                        format!(
                            "unknown ledger setting `{other}` for postgres (use schema, table)"
                        ),
                    ));
                }
            }
        }
        for ident in [&loc.schema, &loc.table] {
            if !is_identifier(ident) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Other,
                    format!(
                        "`{ident}` is not a valid ledger identifier (letters, digits, _; max 50 chars)"
                    ),
                ));
            }
        }
        Ok(loc)
    }

    fn applied(&self) -> String {
        format!("\"{}\".\"{}\"", self.schema, self.table)
    }

    fn history(&self) -> String {
        format!("\"{}\".\"{}_history\"", self.schema, self.table)
    }

    fn meta(&self) -> String {
        format!("\"{}\".\"{}_meta\"", self.schema, self.table)
    }

    /// Advisory-lock key derived from the ledger location.
    pub fn lock_key(&self) -> i64 {
        let digest = Sha256::digest(format!("migrate-db:{}.{}", self.schema, self.table));
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        i64::from_be_bytes(bytes)
    }
}

fn is_identifier(s: &str) -> bool {
    // Leave room for the `_history` suffix within Postgres' 63-byte limit.
    !s.is_empty()
        && s.len() <= 50
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A connection to one PostgreSQL target.
pub struct PostgresProvider {
    config: Config,
    tls: MakeRustlsConnect,
    client: Client,
    lock_client: Option<Client>,
    /// Our holder record while we hold the lock.
    holder: Option<LockHolder>,
    holder_written: bool,
    ledger: LedgerLocation,
    db_user: String,
}

impl std::fmt::Debug for PostgresProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresProvider")
            .field("ledger", &self.ledger)
            .field("db_user", &self.db_user)
            .finish_non_exhaustive()
    }
}

/// Map a driver error to a provider error, keeping SQLSTATE details.
fn map_err(context: &str, e: tokio_postgres::Error) -> ProviderError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code();
        let kind = if code.starts_with("28") {
            ProviderErrorKind::Auth
        } else {
            ProviderErrorKind::Execution
        };
        let mut message = format!("{context}: {} {code}: {}", db.severity(), db.message());
        if let Some(detail) = db.detail() {
            message.push_str(&format!(" (detail: {detail})"));
        }
        if let Some(hint) = db.hint() {
            message.push_str(&format!(" (hint: {hint})"));
        }
        if let Some(tokio_postgres::error::ErrorPosition::Original(pos)) = db.position() {
            message.push_str(&format!(" at character {pos}"));
        }
        return ProviderError::new(kind, message);
    }
    if e.is_closed() {
        return ProviderError::connection(format!("{context}: connection closed"));
    }
    ProviderError::new(ProviderErrorKind::Connection, format!("{context}: {e}"))
}

/// Re-label an error as a ledger error (for ledger reads/writes).
fn ledger_err(context: &str, e: tokio_postgres::Error) -> ProviderError {
    let mut err = map_err(context, e);
    if err.kind == ProviderErrorKind::Execution {
        err.kind = ProviderErrorKind::Ledger;
    }
    err
}

/// The TLS connector: rustls with certificate and host name verification.
/// Roots are the bundled Mozilla set (so a slim container needs no CA files)
/// plus whatever the operating system trusts (private CAs).
fn tls_connector() -> Result<MakeRustlsConnect, ProviderError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // A store that cannot be read only means fewer roots, never weaker checks.
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| ProviderError::connection(format!("cannot set up TLS: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

async fn open(config: &Config, tls: &MakeRustlsConnect) -> Result<Client, ProviderError> {
    let (client, connection) = config
        .connect(tls.clone())
        .await
        .map_err(|e| map_err("cannot connect to PostgreSQL", e))?;
    tokio::spawn(async move {
        // Errors surface on the next client call.
        let _ = connection.await;
    });
    Ok(client)
}

impl PostgresProvider {
    /// Connect. `application_name` identifies this run to DBAs and in lock
    /// diagnostics (SPEC §7 rule 10).
    ///
    /// TLS follows `sslmode` (see the module docs); certificates are always
    /// verified.
    pub async fn connect(
        connection: &str,
        ledger: &toml::Table,
        application_name: &str,
    ) -> Result<Self, ProviderError> {
        let mut config: Config = connection.parse().map_err(|e: tokio_postgres::Error| {
            ProviderError::new(
                ProviderErrorKind::Connection,
                format!("invalid PostgreSQL connection string: {e}"),
            )
        })?;
        // application_name is limited to 63 bytes; keep it ASCII-safe.
        let app: String = application_name
            .chars()
            .filter(char::is_ascii)
            .take(63)
            .collect();
        config.application_name(&app);
        let ledger = LedgerLocation::from_config(ledger)?;
        let tls = tls_connector()?;
        let client = open(&config, &tls).await?;
        let db_user: String = client
            .query_one("SELECT current_user::text", &[])
            .await
            .map_err(|e| map_err("cannot query current_user", e))?
            .get(0);
        Ok(Self {
            config,
            tls,
            client,
            lock_client: None,
            holder: None,
            holder_written: false,
            ledger,
            db_user,
        })
    }

    /// Ledger location in use.
    pub fn ledger_location(&self) -> &LedgerLocation {
        &self.ledger
    }

    async fn ledger_exists(&self) -> Result<bool, ProviderError> {
        let row = self
            .client
            .query_one(
                "SELECT to_regclass($1) IS NOT NULL",
                &[&self.ledger.applied()],
            )
            .await
            .map_err(|e| ledger_err("cannot check for the ledger", e))?;
        Ok(row.get(0))
    }

    async fn lock_holder(&self, key: i64) -> Option<String> {
        let (classid, objid) = split_key(key);
        let row = self
            .client
            .query_opt(
                "SELECT coalesce(a.application_name, '') || ' pid ' || l.pid::text \
                 || coalesce(' from ' || host(a.client_addr), '') \
                 || coalesce(' since ' || to_char(a.backend_start AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), '') \
                 FROM pg_locks l LEFT JOIN pg_stat_activity a ON a.pid = l.pid \
                 WHERE l.locktype = 'advisory' AND l.granted \
                   AND l.classid = $1 AND l.objid = $2 AND l.objsubid = 1 \
                 LIMIT 1",
                &[&classid, &objid],
            )
            .await
            .ok()
            .flatten()?;
        row.get::<_, Option<String>>(0)
    }

    fn applied_by(&self, os_identity: &str) -> String {
        format!("pg:{} / {os_identity}", self.db_user)
    }

    /// Write our holder record if we hold the lock, haven't written it yet,
    /// and the meta table exists. Replaces a stale record.
    async fn write_holder(&mut self) -> Result<(), ProviderError> {
        let Some(holder) = self.holder.as_ref().filter(|_| !self.holder_written) else {
            return Ok(());
        };
        let meta = self.ledger.meta();
        let exists: bool = self
            .client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&meta])
            .await
            .map_err(|e| ledger_err("cannot check for the ledger meta table", e))?
            .get(0);
        if !exists {
            return Ok(());
        }
        self.client
            .execute(
                &format!(
                    "INSERT INTO {meta} (key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
                ),
                &[&LockHolder::META_KEY, &holder.to_record()],
            )
            .await
            .map_err(|e| ledger_err("cannot write the lock holder record", e))?;
        self.holder_written = true;
        Ok(())
    }

    /// The stored holder record, if the meta table and the row exist.
    /// Read-only, and never an error: an unreadable record is "unknown".
    async fn read_holder(&self) -> Option<LockHolder> {
        let meta = self.ledger.meta();
        let exists: bool = self
            .client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&meta])
            .await
            .ok()?
            .get(0);
        if !exists {
            return None;
        }
        let row = self
            .client
            .query_opt(
                &format!("SELECT value FROM {meta} WHERE key = $1"),
                &[&LockHolder::META_KEY],
            )
            .await
            .ok()??;
        LockHolder::from_record(&row.get::<_, String>(0))
    }

    /// The stored holder record as text (for an exact-match delete).
    async fn read_holder_text(&self) -> Result<Option<String>, ProviderError> {
        let meta = self.ledger.meta();
        if !self.table_exists(&meta).await? {
            return Ok(None);
        }
        Ok(self
            .client
            .query_opt(
                &format!("SELECT value FROM {meta} WHERE key = $1"),
                &[&LockHolder::META_KEY],
            )
            .await
            .map_err(|e| ledger_err("cannot read the lock holder record", e))?
            .map(|r| r.get(0)))
    }

    async fn table_exists(&self, name: &str) -> Result<bool, ProviderError> {
        Ok(self
            .client
            .query_one("SELECT to_regclass($1) IS NOT NULL", &[&name])
            .await
            .map_err(|e| ledger_err("cannot check for the ledger", e))?
            .get(0))
    }

    /// Delete the holder record only if it still reads `record`, so a newer
    /// holder's record is never removed. `when_free` also requires the
    /// advisory lock to be free at that moment.
    async fn delete_holder_record(
        &self,
        record: &str,
        when_free: bool,
    ) -> Result<bool, ProviderError> {
        let (classid, objid) = split_key(self.ledger.lock_key());
        let sql = format!(
            "DELETE FROM {} WHERE key = $1 AND value = $2 AND (NOT $3 OR NOT EXISTS ( \
             SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
             AND classid = $4 AND objid = $5 AND objsubid = 1))",
            self.ledger.meta()
        );
        let n = self
            .client
            .execute(
                &sql,
                &[&LockHolder::META_KEY, &record, &when_free, &classid, &objid],
            )
            .await
            .map_err(|e| ledger_err("cannot remove the lock holder record", e))?;
        Ok(n == 1)
    }

    /// Does backend `pid` hold our advisory lock? `pg_locks` is visible to
    /// every role, so this check always works.
    async fn backend_holds_lock(&self, pid: i64) -> Result<bool, ProviderError> {
        let (classid, objid) = split_key(self.ledger.lock_key());
        let pid = i32::try_from(pid).unwrap_or(-1);
        Ok(self
            .client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
                 AND classid = $1 AND objid = $2 AND objsubid = 1 AND pid = $3)",
                &[&classid, &objid, &pid],
            )
            .await
            .map_err(|e| map_err("cannot test the advisory lock", e))?
            .get(0))
    }

    /// Append a `force-unlock` history row, if the history table exists.
    async fn record_force_unlock(&self, identity: &RunIdentity) -> Result<(), ProviderError> {
        let history = self.ledger.history();
        if !self.table_exists(&history).await? {
            return Ok(());
        }
        let now = Utc::now();
        self.client
            .execute(
                &format!(
                    "INSERT INTO {history} (version, direction, event, status, checksum, \
                     started_at, finished_at, applied_by, host, tool_version, error) \
                     VALUES ('', NULL, 'force-unlock', 'succeeded', NULL, $1, $1, $2, $3, $4, NULL)"
                ),
                &[
                    &now,
                    &self.applied_by(&identity.applied_by),
                    &identity.host,
                    &identity.tool_version,
                ],
            )
            .await
            .map(|_| ())
            .map_err(|e| ledger_err("cannot write the history log", e))
    }
}

/// Run a script, and when `cancel` fires ask the server to cancel it (the
/// protocol's cancel request), then wait for the script's own error. The
/// transaction then rolls back through the normal failure path.
async fn cancellable<F>(
    run: F,
    pg_cancel: tokio_postgres::CancelToken,
    tls: MakeRustlsConnect,
    cancel: &CancelToken,
) -> Result<(), tokio_postgres::Error>
where
    F: std::future::Future<Output = Result<(), tokio_postgres::Error>>,
{
    tokio::pin!(run);
    tokio::select! {
        r = &mut run => r,
        () = cancel.cancelled() => {
            let _ = pg_cancel.cancel_query(tls).await;
            run.await
        }
    }
}

/// Split a bigint advisory key the way `pg_locks` reports it.
fn split_key(key: i64) -> (u32, u32) {
    let bits = key as u64;
    ((bits >> 32) as u32, (bits & 0xFFFF_FFFF) as u32)
}

fn row_to_applied(row: &Row) -> Result<AppliedMigration, ProviderError> {
    let kind_text: String = row.get("kind");
    let status_text: String = row.get("status");
    let direction: Option<String> = row.get("direction");
    Ok(AppliedMigration {
        version: row.get("version"),
        description: row.get("description"),
        script_name: row.get("script_name"),
        checksum: row.get("checksum"),
        kind: Kind::parse(&kind_text)
            .ok_or_else(|| ProviderError::ledger(format!("unknown ledger kind `{kind_text}`")))?,
        status: Status::parse(&status_text).ok_or_else(|| {
            ProviderError::ledger(format!("unknown ledger status `{status_text}`"))
        })?,
        direction: match direction.as_deref() {
            None => None,
            Some("up") => Some(Direction::Up),
            Some("down") => Some(Direction::Down),
            Some(other) => {
                return Err(ProviderError::ledger(format!(
                    "unknown ledger direction `{other}`"
                )));
            }
        },
        applied_at: row.get("applied_at"),
        applied_by: row.get("applied_by"),
        execution_ms: row.get("execution_ms"),
        tool_version: row.get("tool_version"),
        executor: row.get("executor"),
        runner_version: row.get("runner_version"),
        op_count: row.get("op_count"),
        completed_ops: row.get("completed_ops"),
        failed_op: row.get("failed_op"),
        recovery: row.get("recovery"),
        lock_epoch: row.get("lock_epoch"),
    })
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[async_trait]
impl Provider for PostgresProvider {
    fn kind(&self) -> &'static str {
        "postgres"
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

    async fn read_ledger(&mut self) -> Result<Vec<AppliedMigration>, ProviderError> {
        if !self.ledger_exists().await? {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT * FROM {} ORDER BY version", self.ledger.applied());
        let rows = self
            .client
            .query(&sql, &[])
            .await
            .map_err(|e| ledger_err("cannot read the ledger", e))?;
        rows.iter().map(row_to_applied).collect()
    }

    async fn ensure_ledger(&mut self) -> Result<(), ProviderError> {
        let l = &self.ledger;
        let ddl = format!(
            "CREATE SCHEMA IF NOT EXISTS \"{schema}\";
CREATE TABLE IF NOT EXISTS {applied} (
    version        text        PRIMARY KEY,
    description    text        NOT NULL,
    script_name    text        NOT NULL,
    checksum       text        NOT NULL,
    kind           text        NOT NULL,
    status         text        NOT NULL,
    direction      text        NULL,
    applied_at     timestamptz NOT NULL,
    applied_by     text        NOT NULL,
    execution_ms   bigint      NOT NULL,
    tool_version   text        NOT NULL,
    executor       text        NOT NULL,
    runner_version text        NULL,
    op_count       integer     NULL,
    completed_ops  integer     NULL,
    failed_op      integer     NULL,
    recovery       text        NULL,
    lock_epoch     bigint      NULL
);
CREATE TABLE IF NOT EXISTS {history} (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    version      text        NOT NULL,
    direction    text        NULL,
    event        text        NOT NULL,
    status       text        NOT NULL,
    checksum     text        NULL,
    started_at   timestamptz NOT NULL,
    finished_at  timestamptz NOT NULL,
    applied_by   text        NOT NULL,
    host         text        NOT NULL,
    tool_version text        NOT NULL,
    from_op      integer     NULL,
    to_op        integer     NULL,
    error        text        NULL
);
CREATE TABLE IF NOT EXISTS {meta} (
    key   text PRIMARY KEY,
    value text NOT NULL
);
INSERT INTO {meta} (key, value) VALUES ('ledger_schema_version', '{LEDGER_SCHEMA_VERSION}')
ON CONFLICT (key) DO NOTHING;",
            schema = l.schema,
            applied = l.applied(),
            history = l.history(),
            meta = l.meta(),
        );
        self.client
            .batch_execute(&ddl)
            .await
            .map_err(|e| ledger_err("cannot create the ledger", e))?;

        let version: String = self
            .client
            .query_one(
                &format!(
                    "SELECT value FROM {} WHERE key = 'ledger_schema_version'",
                    l.meta()
                ),
                &[],
            )
            .await
            .map_err(|e| ledger_err("cannot read the ledger schema version", e))?
            .get(0);
        let found: i64 = version.parse().unwrap_or(i64::MAX);
        if found > LEDGER_SCHEMA_VERSION {
            return Err(ProviderError::new(
                ProviderErrorKind::Ledger,
                format!(
                    "the ledger schema version is {found}, newer than this migrate-db understands ({LEDGER_SCHEMA_VERSION}); upgrade migrate-db"
                ),
            ));
        }
        // First run: the lock was taken before the meta table existed.
        self.write_holder().await
    }

    async fn acquire_lock(&mut self, options: &LockOptions) -> Result<(), ProviderError> {
        if self.lock_client.is_some() {
            return Ok(());
        }
        let lock_client = open(&self.config, &self.tls).await?;
        let key = self.ledger.lock_key();
        let deadline = Instant::now() + options.timeout;
        loop {
            let row = lock_client
                .query_one(
                    "SELECT pg_try_advisory_lock($1), pg_backend_pid()::bigint",
                    &[&key],
                )
                .await
                .map_err(|e| map_err("cannot request the advisory lock", e))?;
            if row.get::<_, bool>(0) {
                self.lock_client = Some(lock_client);
                self.holder = Some(LockHolder {
                    owner: options.owner.clone(),
                    principal: self.db_user.clone(),
                    application: self.config.get_application_name().unwrap_or("").to_owned(),
                    session_id: Some(row.get(1)),
                    acquired_at: Utc::now(),
                });
                // Before the first run the meta table doesn't exist yet;
                // ensure_ledger then writes the record.
                return self.write_holder().await;
            }
            if Instant::now() >= deadline {
                let holder = match self.read_holder().await {
                    Some(h) => Some(h.describe()),
                    None => self.lock_holder(key).await,
                };
                return Err(ProviderError::lock_timeout(holder.as_deref()));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn release_lock(&mut self) -> Result<(), ProviderError> {
        let Some(lock_client) = self.lock_client.take() else {
            return Ok(());
        };
        // Remove our record while still holding the lock, so nobody can
        // acquire in between and have their record deleted.
        let removed = if self.holder_written {
            self.client
                .execute(
                    &format!("DELETE FROM {} WHERE key = $1", self.ledger.meta()),
                    &[&LockHolder::META_KEY],
                )
                .await
                .map(|_| ())
                .map_err(|e| ledger_err("cannot remove the lock holder record", e))
        } else {
            Ok(())
        };
        self.holder = None;
        self.holder_written = false;
        let key = self.ledger.lock_key();
        lock_client
            .query_one("SELECT pg_advisory_unlock($1)", &[&key])
            .await
            .map_err(|e| map_err("cannot release the advisory lock", e))?;
        removed
    }

    async fn lock_status(&mut self) -> Result<LockStatus, ProviderError> {
        let (classid, objid) = split_key(self.ledger.lock_key());
        let held: bool = self
            .client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND granted \
                 AND classid = $1 AND objid = $2 AND objsubid = 1)",
                &[&classid, &objid],
            )
            .await
            .map_err(|e| map_err("cannot test the advisory lock", e))?
            .get(0);
        let record = self.read_holder().await;
        Ok(LockStatus::from_parts(held, record))
    }

    async fn force_unlock(
        &mut self,
        kill_holder: bool,
        identity: &RunIdentity,
    ) -> Result<UnlockOutcome, ProviderError> {
        let status = self.lock_status().await?;
        let record = self.read_holder_text().await?;
        if !status.held {
            let Some(record) = record else {
                return Ok(UnlockOutcome::NothingToDo);
            };
            if !self.delete_holder_record(&record, true).await? {
                return Err(ProviderError::new(
                    ProviderErrorKind::LockHeld,
                    "the lock changed while unlocking (a run may have just started); \
                     check `status` and try again",
                ));
            }
            self.record_force_unlock(identity).await?;
            return Ok(UnlockOutcome::ClearedStale(LockHolder::from_record(
                &record,
            )));
        }
        if !kill_holder {
            return Err(ProviderError::lock_held(status.holder.as_ref()));
        }
        let (Some(holder), Some(record)) = (status.holder, record) else {
            return Err(ProviderError::holder_unverified(
                "no holder record names its session (the holder is starting up right now, \
                 or is an older migrate-db)",
            ));
        };
        let Some(pid) = holder.session_id else {
            return Err(ProviderError::holder_unverified(
                "the holder record has no backend pid",
            ));
        };
        if !self.backend_holds_lock(pid).await? {
            return Err(ProviderError::holder_unverified(&format!(
                "backend {pid} from the holder record doesn't hold it (the record is outdated)"
            )));
        }
        let pid32 = i32::try_from(pid).unwrap_or(-1);
        // Waits (up to 5 s) for the backend to exit, so the lock is free
        // when this returns true.
        let terminated: bool = self
            .client
            .query_one("SELECT pg_terminate_backend($1, 5000)", &[&pid32])
            .await
            .map_err(|e| {
                let mut err = map_err(&format!("cannot terminate backend {pid}"), e);
                // 42501: insufficient_privilege.
                if err.message.contains("42501") {
                    err.kind = ProviderErrorKind::Auth;
                    err.message
                        .push_str(" (needs the pg_signal_backend role, or the holder's role)");
                }
                err
            })?
            .get(0);
        if !terminated {
            return Err(ProviderError::new(
                ProviderErrorKind::LockHeld,
                format!("backend {pid} didn't exit within 5 s; check `status` and try again"),
            ));
        }
        // The terminated backend can't remove its own record any more.
        self.delete_holder_record(&record, false).await?;
        self.record_force_unlock(identity).await?;
        Ok(UnlockOutcome::KilledHolder(holder))
    }

    async fn mark(
        &mut self,
        marks: &[PlannedStep],
        identity: &RunIdentity,
    ) -> Result<(), ProviderError> {
        let applied_by = self.applied_by(&identity.applied_by);
        let now = Utc::now();
        let ledger_insert = format!(
            "INSERT INTO {} (version, description, script_name, checksum, kind, status, direction, \
             applied_at, applied_by, execution_ms, tool_version, executor) \
             VALUES ($1, $2, $3, $4, $5, 'applied', NULL, $6, $7, 0, $8, 'native')",
            self.ledger.applied()
        );
        let history_insert = format!(
            "INSERT INTO {} (version, direction, event, status, checksum, started_at, finished_at, \
             applied_by, host, tool_version, error) \
             VALUES ($1, 'up', 'baseline', 'succeeded', $2, $3, $3, $4, $5, $6, NULL)",
            self.ledger.history()
        );
        let tx = self
            .client
            .transaction()
            .await
            .map_err(|e| map_err("cannot start a transaction", e))?;
        for m in marks {
            let version = m.version.to_string();
            tx.execute(
                &ledger_insert,
                &[
                    &version,
                    &m.description,
                    &m.script_name,
                    &m.checksum,
                    &m.kind.as_str(),
                    &now,
                    &applied_by,
                    &identity.tool_version,
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot write the baseline to the ledger", e))?;
            tx.execute(
                &history_insert,
                &[
                    &version,
                    &m.checksum,
                    &now,
                    &applied_by,
                    &identity.host,
                    &identity.tool_version,
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot write the history log", e))?;
        }
        tx.commit()
            .await
            .map_err(|e| map_err("cannot commit the baseline", e))
    }

    async fn execute(
        &mut self,
        request: &ExecuteRequest<'_>,
    ) -> Result<StepOutcome, ProviderError> {
        let step = request.step;
        let applied_by = self.applied_by(&request.identity.applied_by);
        let started = Utc::now();
        let clock = Instant::now();
        let timeout_ms = request
            .script
            .directives
            .timeout_secs
            .map(|s| s.saturating_mul(1000));

        // `kind` comes from a fixed enum, never from user input.
        let kind = step.kind.as_str();
        let ledger_write = match step.direction {
            Direction::Up => format!(
                "INSERT INTO {} (version, description, script_name, checksum, kind, status, direction, \
                 applied_at, applied_by, execution_ms, tool_version, executor) \
                 VALUES ($1, $2, $3, $4, '{kind}', 'applied', NULL, $5, $6, $7, $8, 'native') \
                 ON CONFLICT (version) DO UPDATE SET description = EXCLUDED.description, \
                 script_name = EXCLUDED.script_name, checksum = EXCLUDED.checksum, kind = EXCLUDED.kind, status = 'applied', \
                 direction = NULL, applied_at = EXCLUDED.applied_at, applied_by = EXCLUDED.applied_by, \
                 execution_ms = EXCLUDED.execution_ms, tool_version = EXCLUDED.tool_version, executor = 'native'",
                self.ledger.applied()
            ),
            Direction::Down => format!("DELETE FROM {} WHERE version = $1", self.ledger.applied()),
        };
        let history_insert = format!(
            "INSERT INTO {} (version, direction, event, status, checksum, started_at, finished_at, \
             applied_by, host, tool_version, error) VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            self.ledger.history()
        );
        let version = step.version.to_string();
        let direction = step.direction.as_str();

        if step.transactional {
            // Taken before the transaction borrows the client.
            let pg_cancel = self.client.cancel_token();
            let tls = self.tls.clone();
            let tx = self
                .client
                .transaction()
                .await
                .map_err(|e| map_err("cannot start a transaction", e))?;
            if let Some(ms) = timeout_ms {
                tx.batch_execute(&format!("SET LOCAL statement_timeout = {ms}"))
                    .await
                    .map_err(|e| map_err("cannot set statement_timeout", e))?;
            }
            let script_result = cancellable(
                tx.batch_execute(&request.script.content),
                pg_cancel,
                tls,
                request.cancel,
            )
            .await;
            if let Err(e) = script_result {
                drop(tx); // rolls back
                let err = map_err(&format!("{} failed", request.script.relative_path), e);
                let finished = Utc::now();
                // Best effort: record the failure outside the rolled-back transaction.
                let _ = self
                    .client
                    .execute(
                        &history_insert,
                        &[
                            &version,
                            &direction,
                            &"failed",
                            &step.checksum,
                            &started,
                            &finished,
                            &applied_by,
                            &request.identity.host,
                            &request.identity.tool_version,
                            &Some(truncate_error(&err.message)),
                        ],
                    )
                    .await;
                return Err(err);
            }
            let elapsed = millis(clock.elapsed());
            let finished = Utc::now();
            match step.direction {
                Direction::Up => {
                    tx.execute(
                        &ledger_write,
                        &[
                            &version,
                            &step.description,
                            &step.script_name,
                            &step.checksum,
                            &finished,
                            &applied_by,
                            &elapsed,
                            &request.identity.tool_version,
                        ],
                    )
                    .await
                }
                Direction::Down => tx.execute(&ledger_write, &[&version]).await,
            }
            .map_err(|e| ledger_err("cannot write the ledger", e))?;
            tx.execute(
                &history_insert,
                &[
                    &version,
                    &direction,
                    &"succeeded",
                    &step.checksum,
                    &started,
                    &finished,
                    &applied_by,
                    &request.identity.host,
                    &request.identity.tool_version,
                    &None::<String>,
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot write the history log", e))?;
            tx.commit()
                .await
                .map_err(|e| map_err("cannot commit the migration", e))?;
            return Ok(StepOutcome {
                duration_ms: elapsed,
            });
        }

        // Non-transactional: the script runs on its own (SPEC §6.3).
        // Mark the row `in_progress` *before* running it, so a run that dies
        // mid-script (agent killed, pipeline cancelled, connection lost)
        // leaves a row that blocks the next run instead of silently
        // re-executing a half-applied script (SPEC §10.4).
        match step.direction {
            Direction::Up => {
                let mark = format!(
                    "INSERT INTO {} (version, description, script_name, checksum, kind, status, direction, \
                     applied_at, applied_by, execution_ms, tool_version, executor) \
                     VALUES ($1, $2, $3, $4, '{kind}', 'in_progress', 'up', $5, $6, 0, $7, 'native') \
                     ON CONFLICT (version) DO UPDATE SET status = 'in_progress', direction = 'up', \
                     applied_at = EXCLUDED.applied_at, applied_by = EXCLUDED.applied_by",
                    self.ledger.applied()
                );
                self.client
                    .execute(
                        &mark,
                        &[
                            &version,
                            &step.description,
                            &step.script_name,
                            &step.checksum,
                            &started,
                            &applied_by,
                            &request.identity.tool_version,
                        ],
                    )
                    .await
            }
            Direction::Down => {
                let mark = format!(
                    "UPDATE {} SET status = 'in_progress', direction = 'down' WHERE version = $1",
                    self.ledger.applied()
                );
                self.client.execute(&mark, &[&version]).await
            }
        }
        .map_err(|e| ledger_err("cannot mark the migration in progress", e))?;

        if let Some(ms) = timeout_ms {
            self.client
                .batch_execute(&format!("SET statement_timeout = {ms}"))
                .await
                .map_err(|e| map_err("cannot set statement_timeout", e))?;
        }
        let script_result = cancellable(
            self.client.batch_execute(&request.script.content),
            self.client.cancel_token(),
            self.tls.clone(),
            request.cancel,
        )
        .await;
        if timeout_ms.is_some() {
            let _ = self.client.batch_execute("RESET statement_timeout").await;
        }
        let elapsed = millis(clock.elapsed());
        let finished = Utc::now();
        if let Err(e) = script_result {
            let err = map_err(&format!("{} failed", request.script.relative_path), e);
            let failed_row = format!(
                "INSERT INTO {} (version, description, script_name, checksum, kind, status, direction, \
                 applied_at, applied_by, execution_ms, tool_version, executor) \
                 VALUES ($1, $2, $3, $4, '{kind}', 'failed', $5, $6, $7, $8, $9, 'native') \
                 ON CONFLICT (version) DO UPDATE SET status = 'failed', direction = EXCLUDED.direction, \
                 applied_at = EXCLUDED.applied_at, applied_by = EXCLUDED.applied_by",
                self.ledger.applied()
            );
            self.client
                .execute(
                    &failed_row,
                    &[
                        &version,
                        &step.description,
                        &step.script_name,
                        &step.checksum,
                        &direction,
                        &finished,
                        &applied_by,
                        &elapsed,
                        &request.identity.tool_version,
                    ],
                )
                .await
                .map_err(|e| ledger_err("cannot record the failed migration", e))?;
            let _ = self
                .client
                .execute(
                    &history_insert,
                    &[
                        &version,
                        &direction,
                        &"failed",
                        &step.checksum,
                        &started,
                        &finished,
                        &applied_by,
                        &request.identity.host,
                        &request.identity.tool_version,
                        &Some(truncate_error(&err.message)),
                    ],
                )
                .await;
            return Err(err);
        }

        let tx = self
            .client
            .transaction()
            .await
            .map_err(|e| map_err("cannot start a transaction", e))?;
        match step.direction {
            Direction::Up => {
                tx.execute(
                    &ledger_write,
                    &[
                        &version,
                        &step.description,
                        &step.script_name,
                        &step.checksum,
                        &finished,
                        &applied_by,
                        &elapsed,
                        &request.identity.tool_version,
                    ],
                )
                .await
            }
            Direction::Down => tx.execute(&ledger_write, &[&version]).await,
        }
        .map_err(|e| ledger_err("cannot write the ledger", e))?;
        tx.execute(
            &history_insert,
            &[
                &version,
                &direction,
                &"succeeded",
                &step.checksum,
                &started,
                &finished,
                &applied_by,
                &request.identity.host,
                &request.identity.tool_version,
                &None::<String>,
            ],
        )
        .await
        .map_err(|e| ledger_err("cannot write the history log", e))?;
        tx.commit()
            .await
            .map_err(|e| map_err("cannot commit the ledger write", e))?;
        Ok(StepOutcome {
            duration_ms: elapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_location_defaults_and_validation() {
        let loc = LedgerLocation::from_config(&toml::Table::new()).unwrap();
        assert_eq!(loc, LedgerLocation::default());
        assert_eq!(loc.applied(), "\"public\".\"__migrations\"");
        assert_eq!(loc.history(), "\"public\".\"__migrations_history\"");

        let custom: toml::Table = toml::from_str("schema = \"ops\"\ntable = \"ledger\"").unwrap();
        let loc = LedgerLocation::from_config(&custom).unwrap();
        assert_eq!(loc.meta(), "\"ops\".\"ledger_meta\"");

        for bad in [
            "schema = \"x\\\"; drop\"",
            "table = \"1abc\"",
            "nope = \"x\"",
            "table = 3",
        ] {
            let t: toml::Table = toml::from_str(bad).unwrap();
            assert!(LedgerLocation::from_config(&t).is_err(), "{bad}");
        }
    }

    #[test]
    fn lock_key_is_stable_and_location_specific() {
        let a = LedgerLocation::default();
        let b = LedgerLocation {
            schema: "ops".into(),
            ..LedgerLocation::default()
        };
        assert_eq!(a.lock_key(), LedgerLocation::default().lock_key());
        assert_ne!(a.lock_key(), b.lock_key());
    }

    #[test]
    fn split_key_matches_pg_locks_layout() {
        assert_eq!(split_key(0x0000_0001_0000_0002), (1, 2));
        assert_eq!(split_key(-1), (u32::MAX, u32::MAX));
    }
}
