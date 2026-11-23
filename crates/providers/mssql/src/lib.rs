//! SQL Server / Azure SQL provider for migrate-db (PROVIDERS.md § SQL Server,
//! ADR-0004).
//!
//! * One connection runs scripts and ledger writes. A second, dedicated
//!   connection holds a session-owned `sp_getapplock` for the whole run.
//! * Scripts are split on `GO` lines ([`batch`]) and sent batch by batch.
//! * Transactional steps run `SET XACT_ABORT ON; BEGIN TRANSACTION`, then every
//!   batch, then the ledger writes, then `COMMIT`, all on one connection.
//!   After **any** error the provider sends `IF @@TRANCOUNT > 0 ROLLBACK`,
//!   because `XACT_ABORT` doesn't cover compile errors (ADR-0004).
//! * Before writing the ledger it checks that the script left exactly the
//!   provider's own transaction open and committable. A script that commits,
//!   rolls back or leaves an extra transaction open fails the step instead of
//!   recording a change that didn't happen, or one that a later disconnect
//!   would roll back.
//! * The ledger is the full SPEC §6.1 shape (`ledger_schema_version = 1`).

#![forbid(unsafe_code)]

pub mod batch;
mod client;
pub mod connstr;
pub mod entra;

use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};

use migrate_db_core::cancel::CancelToken;
use migrate_db_core::model::{AppliedMigration, Direction, Kind, Status, truncate_error};
use migrate_db_core::planner::PlannedStep;
use migrate_db_core::provider::{
    Capabilities, ExecuteRequest, LockHolder, LockOptions, LockStatus, Provider, ProviderError,
    ProviderErrorKind, RunIdentity, StepOutcome, UnlockOutcome,
};
use migrate_db_core::source::ScriptFile;

use crate::batch::Batch;
use crate::client::{Client, DbError, Param, Value};
use crate::connstr::{Authentication, ConnectOptions};

/// Ledger schema version this provider writes.
pub const LEDGER_SCHEMA_VERSION: i64 = 1;

/// Offline checks for one script, without a connection: `validate --offline`
/// calls this directly, and the engine calls it (through
/// [`Provider::prepare`]) for every planned step before anything runs.
/// Today it checks the `GO` batch separators.
pub fn prepare(script: &ScriptFile) -> Result<(), String> {
    batch::split(&script.content).map_err(|e| format!("{}: {e}", script.relative_path))?;
    let uses = batch::sqlcmd_syntax(&script.content);
    if uses.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = uses
        .iter()
        .take(5)
        .map(|u| format!("line {} ({})", u.line, u.what))
        .collect();
    Err(format!(
        "{}: sqlcmd-only syntax at {}{}: the native executor sends batches straight to SQL Server, \
         which can't run it; use ${{placeholders}} instead of :setvar / $(var) and inline :r includes \
         (a sqlcmd executor comes in a later release)",
        script.relative_path,
        list.join(", "),
        if uses.len() > 5 { ", …" } else { "" }
    ))
}

/// Warnings for a repeatable (`R__`) script (SPEC §3.6). Repeatables re-run
/// whenever they change, so they must be idempotent, and they must not hold
/// schema-bound or indexed views.
pub fn lint_repeatable(script: &ScriptFile) -> Vec<String> {
    let code = strip_comments(&script.content).to_ascii_lowercase();
    let words: Vec<&str> = code.split_whitespace().collect();
    let mut warnings = Vec::new();
    let creates = words.windows(2).any(|w| {
        w[0] == "create" && matches!(w[1], "procedure" | "proc" | "view" | "function" | "trigger")
    });
    if creates {
        warnings.push(format!(
            "{}: use CREATE OR ALTER in repeatables; a plain CREATE fails when the file re-applies",
            script.relative_path
        ));
    }
    if words.windows(2).any(|w| w == ["with", "schemabinding"]) {
        warnings.push(format!(
            "{}: WITH SCHEMABINDING in a repeatable: ALTER VIEW drops indexes on indexed views and \
             schema-bound objects block table changes; keep it in a versioned migration",
            script.relative_path
        ));
    }
    warnings
}

/// Code without `--` and `/* */` comments (strings are left alone; good
/// enough for keyword lints).
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut depth = 0u32;
    while let Some(c) = chars.next() {
        match (c, chars.peek().copied()) {
            ('/', Some('*')) => {
                chars.next();
                depth += 1;
            }
            ('*', Some('/')) if depth > 0 => {
                chars.next();
                depth -= 1;
            }
            ('-', Some('-')) if depth == 0 => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    out
}

/// Ledger location (`ledger = { schema = …, table = … }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerLocation {
    /// Schema (default `dbo`).
    pub schema: String,
    /// Applied-set table (default `__migrations`). The history and meta tables
    /// are `<table>_history` and `<table>_meta`.
    pub table: String,
}

impl Default for LedgerLocation {
    fn default() -> Self {
        Self {
            schema: "dbo".into(),
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
                        format!("unknown ledger setting `{other}` for mssql (use schema, table)"),
                    ));
                }
            }
        }
        for ident in [&loc.schema, &loc.table] {
            if !is_identifier(ident) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Other,
                    format!(
                        "`{ident}` is not a valid ledger identifier (letters, digits, _; max 100 chars)"
                    ),
                ));
            }
        }
        Ok(loc)
    }

    fn applied(&self) -> String {
        format!("[{}].[{}]", self.schema, self.table)
    }

    fn history(&self) -> String {
        format!("[{}].[{}_history]", self.schema, self.table)
    }

    fn meta(&self) -> String {
        format!("[{}].[{}_meta]", self.schema, self.table)
    }

    /// `sp_getapplock` resource name. Applocks are scoped to the database.
    pub fn lock_resource(&self) -> String {
        format!("migrate-db:{}.{}", self.schema, self.table)
    }
}

fn is_identifier(s: &str) -> bool {
    // Leave room for the `_history` suffix within the 128-character limit.
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Map a driver error, keeping the server's message number and line.
fn map_err(context: &str, e: &DbError, line_offset: usize) -> ProviderError {
    let kind = match e {
        DbError::Connection(_) => ProviderErrorKind::Connection,
        DbError::Cancelled => ProviderErrorKind::Cancelled,
        _ if e.is_auth() => ProviderErrorKind::Auth,
        _ => ProviderErrorKind::Execution,
    };
    ProviderError::new(kind, format!("{context}: {}", e.describe(line_offset)))
}

/// Re-label an execution error as a ledger error.
fn ledger_err(context: &str, e: &DbError) -> ProviderError {
    let mut err = map_err(context, e, 0);
    if err.kind == ProviderErrorKind::Execution {
        err.kind = ProviderErrorKind::Ledger;
    }
    err
}

/// UTC timestamp as text that `CAST(… AS datetime2(3))` parses exactly.
fn ts(t: DateTime<Utc>) -> Param {
    Param::Text(Some(t.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()))
}

fn text(s: &str) -> Param {
    Param::Text(Some(s.to_owned()))
}

fn millis(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// A connection to one SQL Server target.
pub struct MssqlProvider {
    options: ConnectOptions,
    application_name: String,
    client: Client,
    lock_client: Option<Client>,
    /// Our holder record while we hold the lock.
    holder: Option<LockHolder>,
    holder_written: bool,
    ledger: LedgerLocation,
    db_user: String,
    /// Entra token, reused for the lock connection. Not in `Debug`.
    token: Option<Token>,
}

impl std::fmt::Debug for MssqlProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MssqlProvider")
            .field("options", &self.options)
            .field("ledger", &self.ledger)
            .field("db_user", &self.db_user)
            .finish_non_exhaustive()
    }
}

impl MssqlProvider {
    /// Connect. `auth` is the run's auth mode (`sql` | `entra`, from
    /// `--auth` / `MIGRATE_DB_AUTH`); without it, `Authentication=` in the
    /// connection string decides. Entra tokens come from the credential
    /// chain in [`entra`]. `application_name` identifies this run to DBAs and
    /// in lock diagnostics (SPEC §7 rule 10).
    pub async fn connect(
        connection: &str,
        ledger: &toml::Table,
        application_name: &str,
        auth: Option<&str>,
    ) -> Result<Self, ProviderError> {
        let options = ConnectOptions::parse(connection, auth)
            .map_err(|e| ProviderError::new(ProviderErrorKind::Connection, e.to_string()))?;
        let ledger = LedgerLocation::from_config(ledger)?;
        // program_name is nvarchar(128).
        let application_name: String = application_name.chars().take(128).collect();
        let token = token_for(&options).await?;
        let mut client = open(&options, &application_name, token.as_ref()).await?;
        let db_user = client
            .query("SELECT SUSER_SNAME()")
            .await
            .map_err(|e| map_err("cannot query the login name", &e, 0))?
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.text().map(str::to_owned))
            .unwrap_or_default();
        Ok(Self {
            options,
            application_name,
            client,
            lock_client: None,
            holder: None,
            holder_written: false,
            ledger,
            db_user,
            token,
        })
    }

    /// Ledger location in use.
    pub fn ledger_location(&self) -> &LedgerLocation {
        &self.ledger
    }

    fn applied_by(&self, os_identity: &str) -> String {
        format!("sql:{} / {os_identity}", self.db_user)
    }

    async fn ledger_exists(&mut self) -> Result<bool, ProviderError> {
        let rows = self
            .client
            .query_params(
                "SELECT CASE WHEN OBJECT_ID(@p1, N'U') IS NULL THEN 0 ELSE 1 END",
                vec![text(&self.ledger.applied())],
            )
            .await
            .map_err(|e| ledger_err("cannot check for the ledger", &e))?;
        Ok(rows.first().and_then(|r| r.first()).and_then(Value::int) == Some(1))
    }

    /// Write our holder record if we hold the lock, haven't written it yet,
    /// and the meta table exists. Replaces a stale record.
    async fn write_holder(&mut self) -> Result<(), ProviderError> {
        let Some(holder) = self.holder.as_ref().filter(|_| !self.holder_written) else {
            return Ok(());
        };
        let meta = self.ledger.meta();
        let sql = format!(
            "IF OBJECT_ID(@p3, N'U') IS NULL SELECT 0; ELSE BEGIN \
             UPDATE {meta} SET value = @p2 WHERE [key] = @p1; \
             IF @@ROWCOUNT = 0 INSERT INTO {meta} ([key], value) VALUES (@p1, @p2); \
             SELECT 1; END"
        );
        let rows = self
            .client
            .query_params(
                &sql,
                vec![
                    text(LockHolder::META_KEY),
                    text(&holder.to_record()),
                    text(&meta),
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot write the lock holder record", &e))?;
        self.holder_written = rows.first().and_then(|r| r.first()).and_then(Value::int) == Some(1);
        Ok(())
    }

    /// The stored holder record, if the meta table and the row exist.
    /// Read-only, and never an error: an unreadable record is "unknown".
    async fn read_holder(&mut self) -> Option<LockHolder> {
        LockHolder::from_record(&self.read_holder_text().await?)
    }

    /// The stored holder record as text (for an exact-match delete).
    async fn read_holder_text(&mut self) -> Option<String> {
        let rows = self
            .client
            .query_params(
                &format!(
                    "IF OBJECT_ID(@p2, N'U') IS NOT NULL SELECT value FROM {} WHERE [key] = @p1",
                    self.ledger.meta()
                ),
                vec![text(LockHolder::META_KEY), text(&self.ledger.meta())],
            )
            .await
            .ok()?;
        rows.first()?.first()?.text().map(str::to_owned)
    }

    /// Does session `spid` hold our applock? Needs `VIEW DATABASE
    /// PERFORMANCE STATE` (`VIEW SERVER STATE` on SQL Server) to see other
    /// sessions; without it the answer is `false`, so `--kill-holder` fails
    /// closed.
    async fn session_holds_lock(&mut self, spid: i64) -> bool {
        let rows = self
            .client
            .query_params(
                "SELECT COUNT(*) FROM sys.dm_tran_locks \
                 WHERE request_session_id = @p2 AND resource_type = N'APPLICATION' \
                 AND request_status = N'GRANT' AND request_mode = N'X' \
                 AND resource_database_id = DB_ID() \
                 AND resource_description LIKE N'%' + LEFT(@p1, 32) + N'%'",
                vec![
                    text(&self.ledger.lock_resource()),
                    Param::BigInt(Some(spid)),
                ],
            )
            .await;
        matches!(rows, Ok(r) if r.first().and_then(|r| r.first()).and_then(Value::int).unwrap_or(0) > 0)
    }

    /// Delete the holder record only if it still reads `record`, so a newer
    /// holder's record is never removed. `when_free` also requires the
    /// native lock to be free at that moment. Returns whether a row went.
    async fn delete_holder_record(
        &mut self,
        record: &str,
        when_free: bool,
    ) -> Result<bool, ProviderError> {
        let free = if when_free {
            " AND APPLOCK_TEST(N'public', @p3, N'Exclusive', N'Session') = 1"
        } else {
            ""
        };
        let sql = format!(
            "DELETE FROM {} WHERE [key] = @p1 AND value = @p2{free}; SELECT @@ROWCOUNT;",
            self.ledger.meta()
        );
        let rows = self
            .client
            .query_params(
                &sql,
                vec![
                    text(LockHolder::META_KEY),
                    text(record),
                    text(&self.ledger.lock_resource()),
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot remove the lock holder record", &e))?;
        Ok(rows.first().and_then(|r| r.first()).and_then(Value::int) == Some(1))
    }

    /// Append a `force-unlock` history row, if the history table exists.
    async fn record_force_unlock(&mut self, identity: &RunIdentity) -> Result<(), ProviderError> {
        let now = ts(Utc::now());
        let history = self.ledger.history();
        let sql = format!(
            "IF OBJECT_ID(@p1, N'U') IS NOT NULL INSERT INTO {history} (version, direction, event, \
             status, checksum, started_at, finished_at, applied_by, host, tool_version, error) \
             VALUES (N'', NULL, N'force-unlock', N'succeeded', NULL, CAST(@p2 AS datetime2(3)), \
             CAST(@p2 AS datetime2(3)), @p3, @p4, @p5, NULL)"
        );
        let params = vec![
            text(&history),
            now,
            text(&self.applied_by(&identity.applied_by)),
            text(&identity.host),
            text(&identity.tool_version),
        ];
        self.client
            .exec_params(&sql, params)
            .await
            .map_err(|e| ledger_err("cannot write the history log", &e))
    }

    /// Who holds the lock, if the principal may see it. Needs
    /// `VIEW DATABASE PERFORMANCE STATE`; without it this returns `None`.
    async fn lock_holder(&mut self) -> Option<String> {
        let rows = self
            .client
            .query_params(
                "SELECT TOP (1) CONCAT(s.program_name, N' session ', s.session_id, \
                 N' from ', s.host_name, N' since ', CONVERT(nvarchar(30), s.login_time, 126)) \
                 FROM sys.dm_tran_locks l JOIN sys.dm_exec_sessions s ON s.session_id = l.request_session_id \
                 WHERE l.resource_type = N'APPLICATION' AND l.request_status = N'GRANT' \
                 AND l.resource_database_id = DB_ID() \
                 AND l.resource_description LIKE N'%' + LEFT(@p1, 32) + N'%'",
                vec![text(&self.ledger.lock_resource())],
            )
            .await
            .ok()?;
        rows.first()?.first()?.text().map(str::to_owned)
    }

    /// Record a failed attempt in the history log, outside any transaction.
    /// Best effort: the step's own error is what the caller reports.
    async fn record_failure(&mut self, event: &HistoryRow<'_>, error: &str) {
        let _ = self
            .client
            .exec_params(&self.history_insert(), event.params("failed", Some(error)))
            .await;
    }

    fn history_insert(&self) -> String {
        format!(
            "INSERT INTO {} (version, direction, event, status, checksum, started_at, finished_at, \
             applied_by, host, tool_version, error) VALUES (@p1, @p2, @p2, @p3, @p4, \
             CAST(@p5 AS datetime2(3)), CAST(@p6 AS datetime2(3)), @p7, @p8, @p9, @p10)",
            self.ledger.history()
        )
    }

    /// Insert or update the applied row as `applied`. `kind` comes from a
    /// fixed enum, never from user input, so it is safe in the SQL text.
    fn applied_upsert(&self, kind: Kind) -> String {
        let t = self.ledger.applied();
        let kind = kind.as_str();
        format!(
            "UPDATE {t} SET description = @p2, script_name = @p3, checksum = @p4, kind = N'{kind}', \
             status = N'applied', direction = NULL, applied_at = CAST(@p5 AS datetime2(3)), applied_by = @p6, \
             execution_ms = @p7, tool_version = @p8, executor = N'native' WHERE version = @p1; \
             IF @@ROWCOUNT = 0 INSERT INTO {t} (version, description, script_name, checksum, kind, status, \
             direction, applied_at, applied_by, execution_ms, tool_version, executor) VALUES (@p1, @p2, @p3, \
             @p4, N'{kind}', N'applied', NULL, CAST(@p5 AS datetime2(3)), @p6, @p7, @p8, N'native');"
        )
    }

    /// Write the success ledger rows (inside the caller's transaction).
    async fn write_success(
        &mut self,
        req: &ExecuteRequest<'_>,
        event: &HistoryRow<'_>,
        elapsed: i64,
    ) -> Result<(), ProviderError> {
        let step = req.step;
        let version = step.version.to_string();
        match step.direction {
            Direction::Up => {
                let sql = self.applied_upsert(step.kind);
                self.client
                    .exec_params(
                        &sql,
                        vec![
                            text(&version),
                            text(&step.description),
                            text(&step.script_name),
                            text(&step.checksum),
                            ts(event.finished),
                            text(event.applied_by),
                            Param::BigInt(Some(elapsed)),
                            text(&req.identity.tool_version),
                        ],
                    )
                    .await
            }
            Direction::Down => {
                let sql = format!("DELETE FROM {} WHERE version = @p1", self.ledger.applied());
                self.client.exec_params(&sql, vec![text(&version)]).await
            }
        }
        .map_err(|e| ledger_err("cannot write the ledger", &e))?;
        let sql = self.history_insert();
        self.client
            .exec_params(&sql, event.params("succeeded", None))
            .await
            .map_err(|e| ledger_err("cannot write the history log", &e))
    }

    /// The ledger and history rows of `baseline mark`, inside the caller's
    /// transaction. Nothing is executed apart from these writes.
    async fn write_marks(
        &mut self,
        marks: &[PlannedStep],
        identity: &RunIdentity,
    ) -> Result<(), ProviderError> {
        let applied_by = self.applied_by(&identity.applied_by);
        let now = Utc::now();
        let history = format!(
            "INSERT INTO {} (version, direction, event, status, checksum, started_at, finished_at,              applied_by, host, tool_version, error) VALUES (@p1, N'up', N'baseline', N'succeeded', @p2,              CAST(@p3 AS datetime2(3)), CAST(@p3 AS datetime2(3)), @p4, @p5, @p6, NULL)",
            self.ledger.history()
        );
        for m in marks {
            let version = m.version.to_string();
            let sql = self.applied_upsert(m.kind);
            self.client
                .exec_params(
                    &sql,
                    vec![
                        text(&version),
                        text(&m.description),
                        text(&m.script_name),
                        text(&m.checksum),
                        ts(now),
                        text(&applied_by),
                        Param::BigInt(Some(0)),
                        text(&identity.tool_version),
                    ],
                )
                .await
                .map_err(|e| ledger_err("cannot write the baseline to the ledger", &e))?;
            self.client
                .exec_params(
                    &history,
                    vec![
                        text(&version),
                        text(&m.checksum),
                        ts(now),
                        text(&applied_by),
                        text(&identity.host),
                        text(&identity.tool_version),
                    ],
                )
                .await
                .map_err(|e| ledger_err("cannot write the history log", &e))?;
        }
        Ok(())
    }

    /// Run every batch of a script. On failure, returns the error already
    /// rendered with file line numbers. In a transactional step, a batch that
    /// ends the provider's transaction stops the script right there, so later
    /// batches never run in autocommit mode.
    async fn run_batches(
        &mut self,
        relative_path: &str,
        batches: &[Batch],
        timeout_secs: Option<u32>,
        transactional: bool,
        cancel: &CancelToken,
    ) -> Result<(), Failure> {
        for (index, b) in batches.iter().enumerate() {
            for _ in 0..b.repeat {
                if cancel.is_cancelled() {
                    return Err(Failure::Clean(ProviderError::new(
                        ProviderErrorKind::Cancelled,
                        format!(
                            "{relative_path}: cancelled before batch {} (starting at line {})",
                            index + 1,
                            b.start_line
                        ),
                    )));
                }
                let run = self
                    .client
                    .exec_cancellable(&b.sql, timeout_secs, Some(cancel))
                    .await;
                if let Err(e) = run {
                    let context = if batches.len() == 1 {
                        format!("{relative_path} failed")
                    } else {
                        format!(
                            "{relative_path} failed in batch {} (starting at line {})",
                            index + 1,
                            b.start_line
                        )
                    };
                    return Err(Failure::Clean(map_err(&context, &e, b.start_line - 1)));
                }
                if transactional {
                    let (count, _) = self.transaction_state().await.map_err(Failure::Clean)?;
                    if count == Some(0) {
                        return Err(ended_transaction(relative_path));
                    }
                }
            }
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<(Option<i64>, Option<i64>), ProviderError> {
        let rows = self
            .client
            .query("SELECT @@TRANCOUNT, XACT_STATE()")
            .await
            .map_err(|e| map_err("cannot check the transaction state", &e, 0))?;
        let row = rows.first();
        Ok((
            row.and_then(|r| r.first()).and_then(Value::int),
            row.and_then(|r| r.get(1)).and_then(Value::int),
        ))
    }

    /// The script must leave exactly our transaction open and committable.
    async fn check_transaction_intact(&mut self, relative_path: &str) -> Result<(), Failure> {
        let (count, state) = self.transaction_state().await.map_err(Failure::Clean)?;
        if count == Some(1) && state == Some(1) {
            return Ok(());
        }
        Err(match (count, state) {
            (Some(0), _) => ended_transaction(relative_path),
            (_, Some(-1)) => Failure::Clean(ProviderError::execution(format!(
                "{relative_path} failed: the script left the transaction uncommittable \
                 (an error was caught without being re-thrown); nothing was recorded"
            ))),
            (Some(n), _) if n > 1 => Failure::Clean(ProviderError::execution(format!(
                "{relative_path} failed: the script left {} extra transaction(s) open; nothing was recorded",
                n - 1
            ))),
            _ => Failure::Clean(ProviderError::execution(format!(
                "{relative_path} failed: the transaction state could not be verified; nothing was recorded"
            ))),
        })
    }

    /// Did the script commit our transaction? The marker table is created
    /// inside it, so a rollback removes it and a commit keeps it.
    async fn marker_committed(&mut self) -> bool {
        let rows = self
            .client
            .query(&format!(
                "SELECT CASE WHEN OBJECT_ID(N'tempdb..{TX_MARKER}') IS NULL THEN 0 ELSE 1 END"
            ))
            .await;
        // Unknown counts as committed: fail closed.
        !matches!(rows.as_deref(), Ok([row, ..]) if row.first().and_then(Value::int) == Some(0))
    }

    async fn drop_marker(&mut self) {
        let _ = self
            .client
            .exec(
                &format!(
                    "IF OBJECT_ID(N'tempdb..{TX_MARKER}') IS NOT NULL DROP TABLE {TX_MARKER};"
                ),
                None,
            )
            .await;
    }

    /// Write (or update) the applied row as `failed`: the dirty state of
    /// SPEC §6.3 that blocks later runs until `repair`.
    async fn mark_failed(
        &mut self,
        req: &ExecuteRequest<'_>,
        applied_by: &str,
        finished: DateTime<Utc>,
        elapsed: i64,
    ) -> Result<(), ProviderError> {
        let step = req.step;
        let t = self.ledger.applied();
        let kind = step.kind.as_str();
        let sql = format!(
            "UPDATE {t} SET status = N'failed', direction = @p5, applied_at = CAST(@p6 AS datetime2(3)), \
             applied_by = @p7, execution_ms = @p8 WHERE version = @p1; \
             IF @@ROWCOUNT = 0 INSERT INTO {t} (version, description, script_name, checksum, kind, status, \
             direction, applied_at, applied_by, execution_ms, tool_version, executor) VALUES (@p1, @p2, @p3, \
             @p4, N'{kind}', N'failed', @p5, CAST(@p6 AS datetime2(3)), @p7, @p8, @p9, N'native');"
        );
        self.client
            .exec_params(
                &sql,
                vec![
                    text(&step.version.to_string()),
                    text(&step.description),
                    text(&step.script_name),
                    text(&step.checksum),
                    text(step.direction.as_str()),
                    ts(finished),
                    text(applied_by),
                    Param::BigInt(Some(elapsed)),
                    text(&req.identity.tool_version),
                ],
            )
            .await
            .map_err(|e| ledger_err("cannot record the failed migration", &e))
    }
}

/// Session temp table created inside the provider's transaction (see
/// [`MssqlProvider::marker_committed`]).
const TX_MARKER: &str = "#migrate_db_tx";

/// How a transactional step failed.
enum Failure {
    /// Everything was rolled back; the database is unchanged.
    Clean(ProviderError),
    /// The script ended the provider's transaction itself, so some changes
    /// may be committed. The step is recorded as `failed` (SPEC §6.3).
    Dirty(ProviderError),
}

fn ended_transaction(relative_path: &str) -> Failure {
    Failure::Dirty(ProviderError::execution(format!(
        "{relative_path} failed: the script committed or rolled back the migration's transaction, \
         so later statements ran outside it"
    )))
}

/// Fields of one history row.
struct HistoryRow<'a> {
    version: String,
    direction: &'static str,
    checksum: &'a str,
    started: DateTime<Utc>,
    finished: DateTime<Utc>,
    applied_by: &'a str,
    host: &'a str,
    tool_version: &'a str,
}

impl HistoryRow<'_> {
    fn params(&self, status: &str, error: Option<&str>) -> Vec<Param> {
        vec![
            text(&self.version),
            text(self.direction),
            text(status),
            text(self.checksum),
            ts(self.started),
            ts(self.finished),
            text(self.applied_by),
            text(self.host),
            text(self.tool_version),
            Param::Text(error.map(truncate_error)),
        ]
    }
}

/// An Entra token for this connection string, or `None` for a SQL login.
async fn token_for(options: &ConnectOptions) -> Result<Option<Token>, ProviderError> {
    match options.authentication {
        Authentication::SqlPassword => Ok(None),
        Authentication::Entra => {
            let hint = options.managed_identity_only.then(|| options.user.clone());
            entra::sql_token(hint)
                .await
                .map(|t| Some(Token(t)))
                .map_err(|e| ProviderError::new(ProviderErrorKind::Auth, e))
        }
    }
}

async fn open(
    options: &ConnectOptions,
    application_name: &str,
    token: Option<&Token>,
) -> Result<Client, ProviderError> {
    Client::connect(options, application_name, token.map(|t| t.0.clone()))
        .await
        .map_err(|e| {
            let mut err = map_err("cannot connect to SQL Server", &e, 0);
            if token.is_some() && err.kind == ProviderErrorKind::Auth {
                err.message.push_str(
                    " (the Entra identity got a token but the database rejected it: \
                     check that it has a database user, e.g. CREATE USER [name] FROM EXTERNAL PROVIDER)",
                );
            }
            err
        })
}

/// An Entra access token. Never printed.
struct Token(String);

fn get_text(row: &[Value], i: usize) -> Option<String> {
    row.get(i).and_then(|v| v.text().map(str::to_owned))
}

fn get_int(row: &[Value], i: usize) -> Option<i64> {
    row.get(i).and_then(Value::int)
}

fn required(row: &[Value], i: usize, name: &str) -> Result<String, ProviderError> {
    get_text(row, i).ok_or_else(|| ProviderError::ledger(format!("ledger column `{name}` is NULL")))
}

fn small(row: &[Value], i: usize) -> Option<i32> {
    get_int(row, i).and_then(|n| i32::try_from(n).ok())
}

/// Column order matches `read_ledger`'s SELECT.
fn row_to_applied(row: &[Value]) -> Result<AppliedMigration, ProviderError> {
    let kind_text = required(row, 4, "kind")?;
    let status_text = required(row, 5, "status")?;
    let applied_at_text = required(row, 7, "applied_at")?;
    let applied_at = NaiveDateTime::parse_from_str(&applied_at_text, "%Y-%m-%dT%H:%M:%S%.f")
        .map_err(|e| ProviderError::ledger(format!("bad applied_at `{applied_at_text}`: {e}")))?
        .and_utc();
    Ok(AppliedMigration {
        version: required(row, 0, "version")?,
        description: required(row, 1, "description")?,
        script_name: required(row, 2, "script_name")?,
        checksum: required(row, 3, "checksum")?,
        kind: Kind::parse(&kind_text)
            .ok_or_else(|| ProviderError::ledger(format!("unknown ledger kind `{kind_text}`")))?,
        status: Status::parse(&status_text).ok_or_else(|| {
            ProviderError::ledger(format!("unknown ledger status `{status_text}`"))
        })?,
        direction: match get_text(row, 6).as_deref() {
            None => None,
            Some("up") => Some(Direction::Up),
            Some("down") => Some(Direction::Down),
            Some(other) => {
                return Err(ProviderError::ledger(format!(
                    "unknown ledger direction `{other}`"
                )));
            }
        },
        applied_at,
        applied_by: required(row, 8, "applied_by")?,
        execution_ms: get_int(row, 9).unwrap_or(0),
        tool_version: required(row, 10, "tool_version")?,
        executor: required(row, 11, "executor")?,
        runner_version: get_text(row, 12),
        op_count: small(row, 13),
        completed_ops: small(row, 14),
        failed_op: small(row, 15),
        recovery: get_text(row, 16),
        lock_epoch: get_int(row, 17),
    })
}

#[async_trait]
impl Provider for MssqlProvider {
    fn kind(&self) -> &'static str {
        "mssql"
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
        prepare(script)
    }

    async fn read_ledger(&mut self) -> Result<Vec<AppliedMigration>, ProviderError> {
        if !self.ledger_exists().await? {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT version, description, script_name, checksum, kind, status, direction, \
             CONVERT(nvarchar(30), applied_at, 126), applied_by, execution_ms, tool_version, executor, \
             runner_version, op_count, completed_ops, failed_op, recovery, lock_epoch \
             FROM {} ORDER BY version",
            self.ledger.applied()
        );
        let rows = self
            .client
            .query(&sql)
            .await
            .map_err(|e| ledger_err("cannot read the ledger", &e))?;
        rows.iter().map(|r| row_to_applied(r)).collect()
    }

    async fn ensure_ledger(&mut self) -> Result<(), ProviderError> {
        let l = &self.ledger;
        let ddl = format!(
            "IF SCHEMA_ID(N'{schema}') IS NULL EXEC(N'CREATE SCHEMA [{schema}]');
IF OBJECT_ID(N'{applied}', N'U') IS NULL CREATE TABLE {applied} (
    version        nvarchar(128)  NOT NULL PRIMARY KEY,
    description    nvarchar(400)  NOT NULL,
    script_name    nvarchar(400)  NOT NULL,
    checksum       nvarchar(64)   NOT NULL,
    kind           nvarchar(16)   NOT NULL,
    status         nvarchar(16)   NOT NULL,
    direction      nvarchar(8)    NULL,
    applied_at     datetime2(3)   NOT NULL,
    applied_by     nvarchar(400)  NOT NULL,
    execution_ms   bigint         NOT NULL,
    tool_version   nvarchar(64)   NOT NULL,
    executor       nvarchar(16)   NOT NULL,
    runner_version nvarchar(64)   NULL,
    op_count       int            NULL,
    completed_ops  int            NULL,
    failed_op      int            NULL,
    recovery       nvarchar(16)   NULL,
    lock_epoch     bigint         NULL
);
IF OBJECT_ID(N'{history}', N'U') IS NULL CREATE TABLE {history} (
    id           bigint IDENTITY(1,1) NOT NULL PRIMARY KEY,
    version      nvarchar(128)  NOT NULL,
    direction    nvarchar(8)    NULL,
    event        nvarchar(32)   NOT NULL,
    status       nvarchar(16)   NOT NULL,
    checksum     nvarchar(64)   NULL,
    started_at   datetime2(3)   NOT NULL,
    finished_at  datetime2(3)   NOT NULL,
    applied_by   nvarchar(400)  NOT NULL,
    host         nvarchar(256)  NOT NULL,
    tool_version nvarchar(64)   NOT NULL,
    from_op      int            NULL,
    to_op        int            NULL,
    error        nvarchar(max)  NULL
);
IF OBJECT_ID(N'{meta}', N'U') IS NULL CREATE TABLE {meta} (
    [key]  nvarchar(128) NOT NULL PRIMARY KEY,
    value  nvarchar(max) NOT NULL
);
IF NOT EXISTS (SELECT 1 FROM {meta} WHERE [key] = N'ledger_schema_version')
    INSERT INTO {meta} ([key], value) VALUES (N'ledger_schema_version', N'{LEDGER_SCHEMA_VERSION}');",
            schema = l.schema,
            applied = l.applied(),
            history = l.history(),
            meta = l.meta(),
        );
        self.client
            .exec(&ddl, None)
            .await
            .map_err(|e| ledger_err("cannot create the ledger", &e))?;

        let sql = format!(
            "SELECT value FROM {} WHERE [key] = N'ledger_schema_version'",
            self.ledger.meta()
        );
        let rows = self
            .client
            .query(&sql)
            .await
            .map_err(|e| ledger_err("cannot read the ledger schema version", &e))?;
        let found: i64 = rows
            .first()
            .and_then(|r| get_text(r, 0))
            .and_then(|v| v.parse().ok())
            .unwrap_or(i64::MAX);
        if found > LEDGER_SCHEMA_VERSION {
            return Err(ProviderError::ledger(format!(
                "the ledger schema version is {found}, newer than this migrate-db understands ({LEDGER_SCHEMA_VERSION}); upgrade migrate-db"
            )));
        }
        // First run: the lock was taken before the meta table existed.
        self.write_holder().await
    }

    async fn acquire_lock(&mut self, options: &LockOptions) -> Result<(), ProviderError> {
        if self.lock_client.is_some() {
            return Ok(());
        }
        let mut lock_client =
            open(&self.options, &self.application_name, self.token.as_ref()).await?;
        let wait_ms = i64::try_from(options.timeout.as_millis()).unwrap_or(i64::from(i32::MAX));
        // The server waits up to @LockTimeout, so a cancelled run can stop the
        // wait with ATTENTION instead of polling. @@SPID is the holder's
        // session id for the holder record (and `unlock --kill-holder`).
        let rows = lock_client
            .query_params(
                "DECLARE @r int; EXEC @r = sp_getapplock @Resource = @p1, @LockMode = N'Exclusive', \
                 @LockOwner = N'Session', @LockTimeout = @p2, @DbPrincipal = N'public'; \
                 SELECT @r, CAST(@@SPID AS bigint);",
                vec![
                    text(&self.ledger.lock_resource()),
                    Param::BigInt(Some(wait_ms.min(i64::from(i32::MAX)))),
                ],
            )
            .await
            .map_err(|e| map_err("cannot request the migration lock", &e, 0))?;
        let row = rows.first();
        match row.and_then(|r| r.first()).and_then(Value::int) {
            Some(0 | 1) => {
                self.lock_client = Some(lock_client);
                self.holder = Some(LockHolder {
                    owner: options.owner.clone(),
                    principal: self.db_user.clone(),
                    application: self.application_name.clone(),
                    session_id: row.and_then(|r| get_int(r, 1)),
                    acquired_at: Utc::now(),
                });
                // Before the first run the meta table doesn't exist yet;
                // ensure_ledger then writes the record.
                self.write_holder().await
            }
            Some(-1) => {
                drop(lock_client);
                // The record needs no extra permissions; the DMV is a
                // fallback for holders that predate the record.
                let holder = match self.read_holder().await {
                    Some(h) => Some(h.describe()),
                    None => self.lock_holder().await,
                };
                Err(ProviderError::lock_timeout(holder.as_deref()))
            }
            other => Err(ProviderError::new(
                ProviderErrorKind::Other,
                format!("sp_getapplock failed with result {other:?}"),
            )),
        }
    }

    async fn release_lock(&mut self) -> Result<(), ProviderError> {
        let Some(mut lock_client) = self.lock_client.take() else {
            return Ok(());
        };
        // Remove our record while still holding the lock, so nobody can
        // acquire in between and have their record deleted.
        let removed = if self.holder_written {
            let sql = format!("DELETE FROM {} WHERE [key] = @p1", self.ledger.meta());
            self.client
                .exec_params(&sql, vec![text(LockHolder::META_KEY)])
                .await
                .map_err(|e| ledger_err("cannot remove the lock holder record", &e))
        } else {
            Ok(())
        };
        self.holder = None;
        self.holder_written = false;
        // Closing the session also releases it; release explicitly so a
        // failure is reported rather than silently left to the server.
        lock_client
            .exec_params(
                "EXEC sp_releaseapplock @Resource = @p1, @LockOwner = N'Session', @DbPrincipal = N'public';",
                vec![text(&self.ledger.lock_resource())],
            )
            .await
            .map_err(|e| map_err("cannot release the migration lock", &e, 0))?;
        removed
    }

    async fn lock_status(&mut self) -> Result<LockStatus, ProviderError> {
        // APPLOCK_TEST runs on the script connection, a different session
        // from our own lock connection, so it sees our lock too.
        let rows = self
            .client
            .query_params(
                "SELECT APPLOCK_TEST(N'public', @p1, N'Exclusive', N'Session')",
                vec![text(&self.ledger.lock_resource())],
            )
            .await
            .map_err(|e| map_err("cannot test the migration lock", &e, 0))?;
        let held = rows.first().and_then(|r| r.first()).and_then(Value::int) == Some(0);
        let record = self.read_holder().await;
        Ok(LockStatus::from_parts(held, record))
    }

    async fn force_unlock(
        &mut self,
        kill_holder: bool,
        identity: &RunIdentity,
    ) -> Result<UnlockOutcome, ProviderError> {
        let status = self.lock_status().await?;
        let record = self.read_holder_text().await;
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
        let Some(spid) = holder.session_id else {
            return Err(ProviderError::holder_unverified(
                "the holder record has no session id",
            ));
        };
        if !self.session_holds_lock(spid).await {
            return Err(ProviderError::holder_unverified(&format!(
                "session {spid} from the holder record can't be confirmed as the holder (the \
                 record may be outdated, or this principal lacks VIEW DATABASE PERFORMANCE STATE, \
                 needed to check before KILL)"
            )));
        }
        // KILL takes no parameters; `spid` is an integer, so this can't inject.
        self.client
            .exec(&format!("KILL {spid};"), None)
            .await
            .map_err(|e| {
                let mut err = map_err(&format!("cannot terminate session {spid}"), &e, 0);
                if matches!(&e, DbError::Server(es) if es.iter().any(|s| s.number == 6102)) {
                    err.kind = ProviderErrorKind::Auth;
                    err.message.push_str(
                        " (needs KILL DATABASE CONNECTION on Azure SQL Database, \
                         ALTER ANY CONNECTION on SQL Server)",
                    );
                }
                err
            })?;
        // The killed session can't remove its own record any more.
        self.delete_holder_record(&record, false).await?;
        self.record_force_unlock(identity).await?;
        Ok(UnlockOutcome::KilledHolder(holder))
    }

    async fn mark(
        &mut self,
        marks: &[PlannedStep],
        identity: &RunIdentity,
    ) -> Result<(), ProviderError> {
        self.client
            .exec("SET XACT_ABORT ON; BEGIN TRANSACTION;", None)
            .await
            .map_err(|e| map_err("cannot start a transaction", &e, 0))?;
        let mut result = self.write_marks(marks, identity).await;
        if result.is_ok() {
            result = self
                .client
                .exec("COMMIT TRANSACTION;", None)
                .await
                .map_err(|e| map_err("cannot commit the baseline", &e, 0));
        }
        if result.is_err() {
            let _ = self
                .client
                .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", None)
                .await;
        }
        result
    }

    async fn execute(
        &mut self,
        request: &ExecuteRequest<'_>,
    ) -> Result<StepOutcome, ProviderError> {
        let step = request.step;
        let script = request.script;
        let batches = batch::split(&script.content)
            .map_err(|e| ProviderError::execution(format!("{}: {e}", script.relative_path)))?;
        let timeout_secs = script
            .directives
            .timeout_secs
            .map(|s| u32::try_from(s).unwrap_or(u32::MAX));
        let applied_by = self.applied_by(&request.identity.applied_by);
        let started = Utc::now();
        let clock = Instant::now();
        let version = step.version.to_string();
        let direction = step.direction.as_str();
        let event = |finished| HistoryRow {
            version: version.clone(),
            direction,
            checksum: &step.checksum,
            started,
            finished,
            applied_by: &applied_by,
            host: &request.identity.host,
            tool_version: &request.identity.tool_version,
        };

        if step.transactional {
            self.client
                .exec(
                    &format!(
                        "SET XACT_ABORT ON; \
                         IF OBJECT_ID(N'tempdb..{TX_MARKER}') IS NOT NULL DROP TABLE {TX_MARKER}; \
                         BEGIN TRANSACTION; CREATE TABLE {TX_MARKER} (x bit);"
                    ),
                    None,
                )
                .await
                .map_err(|e| map_err("cannot start a transaction", &e, 0))?;
            let mut result = self
                .run_batches(
                    &script.relative_path,
                    &batches,
                    timeout_secs,
                    true,
                    request.cancel,
                )
                .await;
            if result.is_ok() {
                result = self.check_transaction_intact(&script.relative_path).await;
            }
            let elapsed = millis(clock.elapsed());
            if result.is_ok() {
                result = self
                    .write_success(request, &event(Utc::now()), elapsed)
                    .await
                    .map_err(Failure::Clean);
            }
            if result.is_ok() {
                result = self
                    .client
                    .exec("COMMIT TRANSACTION;", None)
                    .await
                    .map_err(|e| Failure::Clean(map_err("cannot commit the migration", &e, 0)));
            }
            let outcome = match result {
                Ok(()) => Ok(StepOutcome {
                    duration_ms: elapsed,
                }),
                Err(failure) => {
                    // XACT_ABORT doesn't roll back on compile errors (ADR-0004).
                    let _ = self
                        .client
                        .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", None)
                        .await;
                    // A surviving marker means the script committed part of
                    // the work: the database may be changed, so fail closed.
                    let (mut err, dirty) = match failure {
                        Failure::Dirty(e) => (e, true),
                        Failure::Clean(e) => {
                            let dirty = self.marker_committed().await;
                            (e, dirty)
                        }
                    };
                    let finished = Utc::now();
                    if dirty {
                        err.message.push_str(
                            "; some changes may be committed, so the migration is marked failed: \
                             check the database, then run `repair` (use -- migrate:no-transaction \
                             for scripts that manage their own transactions)",
                        );
                        if let Err(e) = self
                            .mark_failed(request, &applied_by, finished, elapsed)
                            .await
                        {
                            err.message.push_str(&format!("; ALSO {}", e.message));
                        }
                    }
                    self.record_failure(&event(finished), &err.message).await;
                    Err(err)
                }
            };
            self.drop_marker().await;
            let _ = self.client.exec("SET XACT_ABORT OFF;", None).await;
            return outcome;
        }

        // Non-transactional (SPEC §6.3). Mark the row `in_progress` *before*
        // running, so a run that dies mid-script leaves a row that blocks the
        // next run instead of silently re-running a half-applied script.
        let t = self.ledger.applied();
        let kind = step.kind.as_str();
        let (mark_sql, mark_params) = match step.direction {
            Direction::Up => (
                format!(
                    "UPDATE {t} SET status = N'in_progress', direction = N'up', applied_at = CAST(@p5 AS datetime2(3)), \
                     applied_by = @p6 WHERE version = @p1; \
                     IF @@ROWCOUNT = 0 INSERT INTO {t} (version, description, script_name, checksum, kind, status, \
                     direction, applied_at, applied_by, execution_ms, tool_version, executor) VALUES (@p1, @p2, @p3, \
                     @p4, N'{kind}', N'in_progress', N'up', CAST(@p5 AS datetime2(3)), @p6, 0, @p7, N'native');"
                ),
                vec![
                    text(&version),
                    text(&step.description),
                    text(&step.script_name),
                    text(&step.checksum),
                    ts(started),
                    text(&applied_by),
                    text(&request.identity.tool_version),
                ],
            ),
            Direction::Down => (
                format!(
                    "UPDATE {t} SET status = N'in_progress', direction = N'down' WHERE version = @p1"
                ),
                vec![text(&version)],
            ),
        };
        self.client
            .exec_params(&mark_sql, mark_params)
            .await
            .map_err(|e| ledger_err("cannot mark the migration in progress", &e))?;

        let script_result = self
            .run_batches(
                &script.relative_path,
                &batches,
                timeout_secs,
                false,
                request.cancel,
            )
            .await;
        let elapsed = millis(clock.elapsed());
        let finished = Utc::now();
        if let Err(Failure::Clean(err) | Failure::Dirty(err)) = script_result {
            // A script that opened its own transaction must not keep it open.
            let _ = self
                .client
                .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", None)
                .await;
            self.mark_failed(request, &applied_by, finished, elapsed)
                .await?;
            self.record_failure(&event(finished), &err.message).await;
            return Err(err);
        }

        self.client
            .exec("BEGIN TRANSACTION;", None)
            .await
            .map_err(|e| map_err("cannot start a transaction", &e, 0))?;
        let mut result = self.write_success(request, &event(finished), elapsed).await;
        if result.is_ok() {
            result = self
                .client
                .exec("COMMIT TRANSACTION;", None)
                .await
                .map_err(|e| map_err("cannot commit the ledger write", &e, 0));
        }
        if let Err(err) = result {
            // The row stays `in_progress`, which blocks later runs: fail closed.
            let _ = self
                .client
                .exec("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;", None)
                .await;
            return Err(err);
        }
        Ok(StepOutcome {
            duration_ms: elapsed,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn ledger_location_defaults_and_validation() {
        let loc = LedgerLocation::from_config(&toml::Table::new()).unwrap();
        assert_eq!(loc, LedgerLocation::default());
        assert_eq!(loc.applied(), "[dbo].[__migrations]");
        assert_eq!(loc.history(), "[dbo].[__migrations_history]");
        assert_eq!(loc.lock_resource(), "migrate-db:dbo.__migrations");

        let custom: toml::Table = toml::from_str("schema = \"ops\"\ntable = \"ledger\"").unwrap();
        assert_eq!(
            LedgerLocation::from_config(&custom).unwrap().meta(),
            "[ops].[ledger_meta]"
        );

        for bad in [
            "schema = \"x]; drop\"",
            "table = \"1abc\"",
            "nope = \"x\"",
            "table = 3",
        ] {
            let t: toml::Table = toml::from_str(bad).unwrap();
            assert!(LedgerLocation::from_config(&t).is_err(), "{bad}");
        }
    }

    #[test]
    fn repeatable_lints() {
        let s = |body: &str| ScriptFile {
            path: "R__x.sql".into(),
            relative_path: "R__x.sql".into(),
            file_name: "R__x.sql".into(),
            extension: "sql".into(),
            content: body.into(),
            checksum: migrate_db_core::checksum::Checksum::of(body.as_bytes()),
            directives: Default::default(),
        };
        // (body, expected warning count)
        for (body, n) in [
            ("CREATE OR ALTER PROCEDURE dbo.p AS SELECT 1;", 0),
            ("create   or alter view dbo.v as select 1 as x;", 0),
            ("CREATE PROCEDURE dbo.p AS SELECT 1;", 1),
            (
                "-- CREATE PROCEDURE in a comment\nCREATE OR ALTER PROC dbo.p AS SELECT 1;",
                0,
            ),
            (
                "/* CREATE VIEW */ CREATE OR ALTER VIEW dbo.v AS SELECT 1 AS x;",
                0,
            ),
            (
                "MERGE dbo.ref AS t USING (VALUES (1)) AS s(id) ON t.id = s.id WHEN NOT MATCHED THEN INSERT (id) VALUES (s.id);",
                0,
            ),
            (
                "CREATE OR ALTER VIEW dbo.v WITH SCHEMABINDING AS SELECT 1 AS x;",
                1,
            ),
            ("CREATE VIEW dbo.v WITH SCHEMABINDING AS SELECT 1 AS x;", 2),
        ] {
            assert_eq!(lint_repeatable(&s(body)).len(), n, "{body}");
        }
    }

    #[test]
    fn timestamps_round_trip_through_style_126() {
        let t = DateTime::parse_from_rfc3339("2026-11-17T21:42:29.950Z")
            .unwrap()
            .with_timezone(&Utc);
        let Param::Text(Some(sent)) = ts(t) else {
            unreachable!()
        };
        assert_eq!(sent, "2026-11-17T21:42:29.950");
        // Style 126 drops a zero fraction; both forms must parse.
        for back in ["2026-11-17T21:42:29.950", "2026-11-17T21:42:29"] {
            assert!(NaiveDateTime::parse_from_str(back, "%Y-%m-%dT%H:%M:%S%.f").is_ok());
        }
    }
}
